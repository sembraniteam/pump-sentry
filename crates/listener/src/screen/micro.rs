//! Early market-microstructure signals from the confirming tape.
//!
//! Pure computation over already-observed swaps: no new RPC, no extra
//! subscriptions. Every signal derives from the confirming round's swap
//! records plus the launch identities (dev/creator) and the previous
//! round's tape point for velocity. Ratios are `Option` — an empty or
//! one-sided tape reads as unknown, never as zero demand.
//!
//! What this deliberately does NOT do: funder-graph linkage per round
//! (the creation-slot bundle screen owns related-wallet forensics;
//! per-round graphs would multiply RPC reads by watchlist size), and
//! liquidity *change* (rounds observe swaps, not reserves — the level
//! already feeds risk).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

use super::{SwapRecord, SwapSide};

/// One round's tape point for velocity: observation time plus raw buy
/// records since the previous watermark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapePoint {
    /// Observation time (unix seconds).
    pub ts: u64,
    /// Buy swap records since the previous watermark.
    pub buys: u32,
}

/// Confirm-tape microstructure snapshot. Counts and sums cover the
/// confirming round's observed swaps (both sides unless noted).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MicroSignals {
    /// Buy swap records (repeats count: activity, not quorum).
    pub buys: u32,
    /// Sell swap records.
    pub sells: u32,
    /// `buys / (buys + sells)` (`None` on an empty tape). High values
    /// alone are NOT organic-buying proof — wash quoting prints buys
    /// too; read with volume and unique traders.
    pub buy_ratio: Option<f64>,
    /// Bought base units.
    pub buy_volume_base: u64,
    /// Sold base units.
    pub sell_volume_base: u64,
    /// Distinct transacting wallets (both sides).
    pub unique_traders: u32,
    /// Largest single wallet's share of round base volume, percent
    /// (`None` on an empty tape). A no-RPC concentration proxy — not
    /// related-wallet forensics (see module docs).
    pub top_wallet_share_pct: Option<f64>,
    /// Signed dev+creator flow in base units (buys minus sells,
    /// saturated): negative means insiders distributed into the round.
    pub creator_net_base: i64,
    /// Buyer-count acceleration: `(buys_now − buys_prev) / secs`
    /// (`None` without a previous tape point or on a zero-length
    /// round gap). Positive means the tape is speeding up into confirm.
    pub velocity_delta: Option<f64>,
}

/// Tally the confirming round. `dev`/`creator` are launch identities
/// (base58, already validated upstream — unparseable entries match no
/// wallet instead of failing the tally). `prev` is the stored tape
/// point from the previous round; `now_secs` stamps this round.
pub fn micro_signals(
    swaps: &[SwapRecord],
    dev: &str,
    creator: Option<&str>,
    prev: Option<TapePoint>,
    now_secs: u64,
) -> MicroSignals {
    let dev_key = dev.parse::<Pubkey>().ok();
    let creator_key = creator.and_then(|c| c.parse::<Pubkey>().ok());
    let is_insider = |w: &Pubkey| Some(*w) == dev_key || Some(*w) == creator_key;

    let mut buys = 0u32;
    let mut sells = 0u32;
    let mut buy_volume = 0u64;
    let mut sell_volume = 0u64;
    let mut unique = HashSet::new();
    let mut per_wallet: HashMap<Pubkey, u64> = HashMap::new();
    let mut creator_net: i128 = 0;
    for s in swaps {
        unique.insert(s.wallet);
        if s.amount_base_units > 0 {
            let entry = per_wallet.entry(s.wallet).or_insert(0);
            *entry = entry.saturating_add(s.amount_base_units);
        }
        match s.side {
            SwapSide::Buy => {
                buys = buys.saturating_add(1);
                buy_volume = buy_volume.saturating_add(s.amount_base_units);
                if is_insider(&s.wallet) {
                    creator_net = creator_net.saturating_add(s.amount_base_units as i128);
                }
            }
            SwapSide::Sell => {
                sells = sells.saturating_add(1);
                sell_volume = sell_volume.saturating_add(s.amount_base_units);
                if is_insider(&s.wallet) {
                    creator_net = creator_net.saturating_sub(s.amount_base_units as i128);
                }
            }
            SwapSide::Unknown => {}
        }
    }
    let total = buys.saturating_add(sells);
    let buy_ratio = if total > 0 {
        Some(buys as f64 / total as f64)
    } else {
        None
    };
    let round_volume = buy_volume.saturating_add(sell_volume);
    let top_wallet_share_pct = if round_volume > 0 {
        per_wallet
            .values()
            .max()
            .map(|m| *m as f64 / round_volume as f64 * 100.0)
            .filter(|v| v.is_finite())
    } else {
        None
    };
    let velocity_delta = match prev {
        Some(point) => {
            let dt = now_secs.saturating_sub(point.ts);
            if dt > 0 {
                let dv = buys as f64 - point.buys as f64;
                Some(dv / dt as f64)
            } else {
                None
            }
        }
        None => None,
    };
    MicroSignals {
        buys,
        sells,
        buy_ratio,
        buy_volume_base: buy_volume,
        sell_volume_base: sell_volume,
        unique_traders: unique.len() as u32,
        top_wallet_share_pct,
        creator_net_base: creator_net.clamp(i64::MIN as i128, i64::MAX as i128) as i64,
        velocity_delta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wallet(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    fn rec(wallet: Pubkey, side: SwapSide, amount: u64) -> SwapRecord {
        SwapRecord {
            signature: "s".to_string(),
            slot: 1001,
            wallet,
            side,
            amount_base_units: amount,
        }
    }

    #[test]
    fn micro_tallies_both_sides_and_identities() {
        let dev = wallet(7);
        let creator = wallet(9);
        let swaps = vec![
            rec(wallet(11), SwapSide::Buy, 1_000_000),
            rec(wallet(12), SwapSide::Buy, 3_000_000),
            rec(wallet(13), SwapSide::Sell, 500_000),
            // Insiders: dev buys 2M, creator dumps 1M → net +1M.
            rec(dev, SwapSide::Buy, 2_000_000),
            rec(creator, SwapSide::Sell, 1_000_000),
            // Undecodable amount adds activity, never volume.
            rec(wallet(14), SwapSide::Buy, 0),
        ];
        let m = micro_signals(
            &swaps,
            &dev.to_string(),
            Some(&creator.to_string()),
            None,
            2_000,
        );
        assert_eq!((m.buys, m.sells), (4, 2));
        assert_eq!(m.buy_ratio, Some(4.0 / 6.0));
        assert_eq!(m.buy_volume_base, 6_000_000);
        assert_eq!(m.sell_volume_base, 1_500_000);
        assert_eq!(m.unique_traders, 6);
        // Top wallet (12: 3M of 7.5M) holds 40%.
        assert_eq!(m.top_wallet_share_pct, Some(40.0));
        assert_eq!(m.creator_net_base, 1_000_000);
        // No previous tape point: velocity unknown, never zero.
        assert_eq!(m.velocity_delta, None);
    }

    #[test]
    fn micro_velocity_is_buyer_acceleration() {
        let swaps = vec![rec(wallet(11), SwapSide::Buy, 1_000_000); 5];
        let dev = wallet(7).to_string();
        // 5 buys now vs 2 buys 10s ago: +0.3 buyers/s.
        let m = micro_signals(
            &swaps,
            &dev,
            None,
            Some(TapePoint { ts: 1_990, buys: 2 }),
            2_000,
        );
        assert_eq!(m.velocity_delta, Some(0.3));
        // Zero-length round gap: unknown, never infinite.
        let m = micro_signals(
            &swaps,
            &dev,
            None,
            Some(TapePoint { ts: 2_000, buys: 2 }),
            2_000,
        );
        assert_eq!(m.velocity_delta, None);
    }

    #[test]
    fn micro_empty_tape_is_unknown_not_zero_demand() {
        let m = micro_signals(&[], &wallet(7).to_string(), None, None, 2_000);
        assert_eq!((m.buys, m.sells), (0, 0));
        assert_eq!(m.buy_ratio, None);
        assert_eq!(m.top_wallet_share_pct, None);
        assert_eq!(m.unique_traders, 0);
        assert_eq!(m.creator_net_base, 0);
    }

    #[test]
    fn micro_unparseable_identities_match_nobody() {
        let swaps = vec![rec(wallet(11), SwapSide::Sell, 1_000_000)];
        // Garbage identity strings never fail the tally and never
        // attribute flow: the dump counts, but to no insider.
        let m = micro_signals(&swaps, "not-a-key", Some("also-bad"), None, 2_000);
        assert_eq!((m.buys, m.sells), (0, 1));
        assert_eq!(m.creator_net_base, 0);
    }
}
