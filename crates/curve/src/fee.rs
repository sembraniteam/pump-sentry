//! Venue-fee read for the paper ledger.
//! Returns `None` on any failure (caller keeps 125 bps).

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Deadline for fee-input reads.
pub const FEE_TIMEOUT: Duration = Duration::from_secs(10);
/// FeeConfig/Global cache TTL. Short on purpose: a schedule change
/// must reach paper within minutes (stale tiers misprice paper the
/// same way they loosen live floors), and a cold read is only 2 RPCs
/// on the rare confirm path. Same value as the dispatch twin
/// (`executor::FEE_INPUTS_TTL`).
pub const FEE_CACHE_TTL: Duration = Duration::from_secs(300);

/// Cached globals: FeeConfig bytes, creator-fee gate, fetch time.
pub type FeeInputsCache = Arc<Mutex<Option<(Vec<u8>, bool, Instant)>>>;

/// Mint supply cache. Supplies never change on bonding curves (fixed at
/// creation; gated mints have no live mint authority), so entries never
/// expire — same bound discipline as the decimals cache (oldest
/// first-seen evicted past the cap, not a random key).
pub type SupplyCache = Arc<Mutex<HashMap<String, (u64, Instant)>>>;

/// Quote-mint class selecting the fee schedule.
/// Canonical single source: `executor::quote` re-exports this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteKind {
    /// SOL-like: `fee_tiers`.
    SolLike,
    /// USDC: `stable_fee_tiers`.
    Usdc,
    /// Else exotic-or-flat.
    Other,
}

/// Total venue fee from curve bytes (owner must be Pump).
pub async fn venue_fee_for_data(
    rpc: &RpcClient,
    cache: &FeeInputsCache,
    supply_cache: &SupplyCache,
    mint: &Pubkey,
    owner: &Pubkey,
    data: &[u8],
) -> Option<f64> {
    if *owner != ids::PUMP_PROGRAM_ID {
        return None;
    }
    let view = curve_fee_inputs(data)?;
    let supply = if view.is_mayhem {
        // Stored total is fresh on every tick read (no extra RPC on
        // the hot path). Dispatch tiers use live mint supply instead
        // (see executor `quote_*_guard`); the two agree except across
        // mayhem tier boundaries (bounded, documented residual).
        view.stored_supply
    } else {
        mint_supply(rpc, supply_cache, mint).await?
    };
    let (fee_bytes, gate_on) = cached_fee_inputs(rpc, cache).await?;
    let schedule = parse_schedule(&fee_bytes)?;
    let (proto, creator) =
        select_tier(&schedule, view.kind, market_cap(view.vq, view.vt, supply)?)?;
    let coin = effective_creator_bps(
        view.coin_override,
        creator,
        gate_on,
        view.creator_is_default,
    );
    Some(effective_total(proto, coin)? as f64)
}

/// Creator leg in whole bps, with both program rules applied: the
/// per-coin (or per-pool) override wins while the creator-fee gate is
/// on and nonzero, and a creatorless curve (or pool) charges no creator
/// fee at all. Canonical single source: `executor::quote` calls this
/// for curve and PumpSwap quotes, so paper and dispatch can never
/// disagree on the creator leg.
pub fn effective_creator_bps(
    override_bps: u64,
    tier_creator_bps: u64,
    gate_on: bool,
    creator_is_default: bool,
) -> u64 {
    if creator_is_default {
        return 0;
    }
    if gate_on && override_bps != 0 {
        override_bps
    } else {
        tier_creator_bps
    }
}

/// Venue total; `None` past 100%.
fn effective_total(proto: u64, creator: u64) -> Option<u64> {
    let total = proto.checked_add(creator)?;
    if total > ids::BPS_DENOMINATOR {
        return None;
    }
    Some(total)
}

/// Joined FeeConfig + Global read behind the TTL cache.
async fn cached_fee_inputs(rpc: &RpcClient, cache: &FeeInputsCache) -> Option<(Vec<u8>, bool)> {
    if let Some((bytes, gate, at)) = cache.lock().await.clone()
        && at.elapsed() < FEE_CACHE_TTL
    {
        return Some((bytes, gate));
    }
    let (fee, global) = tokio::time::timeout(FEE_TIMEOUT, async {
        tokio::join!(
            rpc.get_account(&ids::FEE_CONFIG_ACCOUNT),
            rpc.get_account(&ids::GLOBAL_ACCOUNT)
        )
    })
    .await
    .ok()?;
    let (fee, global) = (fee.ok()?, global.ok()?);
    if fee.owner != ids::FEE_CONFIG_PROGRAM || global.owner != ids::PUMP_PROGRAM_ID {
        return None;
    }
    let gate = parse_fee_gate(&global.data)?;
    *cache.lock().await = Some((fee.data.clone(), gate, Instant::now()));
    Some((fee.data, gate))
}

/// Creator-fee gate byte (0/1) from Global account data. `None` on
/// short/bad bytes. Canonical single source: dispatch's fee-input fetch
/// calls this too.
pub fn parse_fee_gate(data: &[u8]) -> Option<bool> {
    match data.get(ids::GLOBAL_FEE_GATE_OFFSET) {
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => None,
    }
}

/// Curve fee inputs for tier selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CurveFeeView {
    kind: QuoteKind,
    coin_override: u64,
    vt: u64,
    vq: u64,
    stored_supply: u64,
    is_mayhem: bool,
    /// Creator is the default key: the program charges no creator fee.
    creator_is_default: bool,
}

fn curve_fee_inputs(data: &[u8]) -> Option<CurveFeeView> {
    if data.len() < ids::CURVE_COIN_CREATOR_FEE_END {
        return None;
    }
    let creator: [u8; 32] = data
        .get(ids::CURVE_CREATOR_OFFSET..ids::CURVE_CREATOR_END)?
        .try_into()
        .ok()?;
    let raw: [u8; 32] = data
        .get(ids::CURVE_QUOTE_MINT_OFFSET..ids::CURVE_QUOTE_MINT_END)?
        .try_into()
        .ok()?;
    let quote = Pubkey::new_from_array(raw);
    // Zero key and WSOL both mean SOL-paired.
    let kind = if quote == Pubkey::default() || quote == ids::WSOL_MINT {
        QuoteKind::SolLike
    } else if quote == ids::USDC_MINT {
        QuoteKind::Usdc
    } else {
        QuoteKind::Other
    };
    let coin = u64le(data, ids::CURVE_COIN_CREATOR_FEE_OFFSET)?;
    Some(CurveFeeView {
        kind,
        coin_override: coin,
        vt: u64le(data, 8)?,
        vq: u64le(data, 16)?,
        stored_supply: u64le(data, crate::layout::TOKEN_SUPPLY_OFFSET)?,
        is_mayhem: data
            .get(ids::CURVE_MAYHEM_FLAG_OFFSET)
            .copied()
            .unwrap_or(0)
            != 0,
        creator_is_default: creator == [0u8; 32],
    })
}

/// Mint supply for fee-tier market caps (cached: immutable on curves).
async fn mint_supply(rpc: &RpcClient, cache: &SupplyCache, mint: &Pubkey) -> Option<u64> {
    let key = mint.to_string();
    if let Some((hit, _)) = cache.lock().await.get(&key) {
        return Some(*hit);
    }
    let supply = tokio::time::timeout(FEE_TIMEOUT, rpc.get_token_supply(mint))
        .await
        .ok()?
        .ok()?;
    let supply = supply.amount.parse::<u64>().ok()?;
    let mut guard = cache.lock().await;
    if guard.len() >= crate::ticker::DECIMALS_CACHE_CAP
        && let Some(oldest) = guard
            .iter()
            .min_by_key(|(_, (_, at))| *at)
            .map(|(k, _)| k.clone())
    {
        guard.remove(&oldest);
    }
    guard.insert(key, (supply, Instant::now()));
    Some(supply)
}

/// Bonding-curve market cap in lamports: `vq * supply / vt`.
/// Canonical single source: `executor::quote::market_cap` wraps this
/// (converting `None` to `QuoteError`). `None` on zero `vt` or overflow.
pub fn market_cap(vq: u64, vt: u64, supply: u64) -> Option<u128> {
    if vt == 0 {
        return None;
    }
    u128::from(vq)
        .checked_mul(u128::from(supply))?
        .checked_div(u128::from(vt))
}

/// Fee triple in bps (IDL order: lp, protocol, creator).
/// Canonical single source: `executor::quote` re-exports this type
/// (no duplicate struct). `lp` is unused on curves but kept so one
/// schedule serves both the paper ledger and dispatch tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeesBps {
    /// LP fee (unused on curves; parsed for completeness).
    pub lp: u64,
    pub protocol: u64,
    pub creator: u64,
}

/// One fee tier: threshold (lamports) + fees above it.
pub type FeeTierRow = (u128, FeesBps);

/// Full fee schedule, picked by market cap.
/// Canonical single source (see [`FeesBps`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FeeSchedule {
    /// SOL-like tiers.
    pub tiers: Vec<FeeTierRow>,
    /// USDC tiers.
    pub stable: Vec<FeeTierRow>,
    /// Flat fallback.
    pub flat: FeesBps,
    /// Exotic flat (all-zero = unset -> flat).
    pub exotic: FeesBps,
}

/// Tier-table bound (DoS cap on hostile lengths; single source is
/// `ids::MAX_FEE_TIERS`).
use ids::MAX_FEE_TIERS;

/// Highest tier at or below mcap, else the first. `None` when empty.
/// Canonical single source: `executor::quote` re-uses this for curve and
/// PumpSwap tier picks.
pub fn tier_for(tiers: &[FeeTierRow], mcap: u128) -> Option<FeesBps> {
    let first = tiers.first()?;
    if mcap < first.0 {
        return Some(first.1);
    }
    let mut picked = first.1;
    for (threshold, fees) in tiers.iter() {
        if mcap >= *threshold {
            picked = *fees;
        }
    }
    Some(picked)
}

/// `(protocol, creator)` for one quote kind at `mcap`.
pub fn select_tier(schedule: &FeeSchedule, kind: QuoteKind, mcap: u128) -> Option<(u64, u64)> {
    let fees = match kind {
        QuoteKind::SolLike => tier_for(&schedule.tiers, mcap)?,
        QuoteKind::Usdc => tier_for(&schedule.stable, mcap)?,
        QuoteKind::Other => {
            let e = schedule.exotic;
            if e.lp == 0 && e.protocol == 0 && e.creator == 0 {
                schedule.flat
            } else {
                e
            }
        }
    };
    Some((fees.protocol, fees.creator))
}

/// Decode the schedule from FeeConfig bytes. `None` on malformed
/// bytes. Canonical single source: dispatch's fee-input fetch uses this
/// too.
pub fn parse_schedule(data: &[u8]) -> Option<FeeSchedule> {
    const TIERS_OFF: usize = 8 + 1 + 32 + 24;
    let flat = FeesBps {
        lp: u64le(data, 41)?,
        protocol: u64le(data, 49)?,
        creator: u64le(data, 57)?,
    };
    let (tiers, at) = parse_tier_vec(data, TIERS_OFF)?;
    let (stable, at) = parse_tier_vec(data, at)?;
    let exotic = if data.len() >= at + 24 {
        FeesBps {
            lp: u64le(data, at)?,
            protocol: u64le(data, at + 8)?,
            creator: u64le(data, at + 16)?,
        }
    } else if data.len() == at {
        FeesBps::default()
    } else {
        return None;
    };
    Some(FeeSchedule {
        tiers,
        stable,
        flat,
        exotic,
    })
}

/// One tier table at `off` plus the next offset. Thresholds must
/// ascend (tier selection assumes it); anything else fails closed.
fn parse_tier_vec(data: &[u8], off: usize) -> Option<(Vec<FeeTierRow>, usize)> {
    let n = u32le(data, off)? as usize;
    if n > MAX_FEE_TIERS {
        return None;
    }
    let mut out = Vec::with_capacity(n.min(16));
    let mut at = off + 4;
    let mut prev = 0u128;
    for _ in 0..n {
        let threshold = data
            .get(at..at + 16)
            .and_then(|b| <[u8; 16]>::try_from(b).ok())
            .map(u128::from_le_bytes)?;
        if threshold < prev {
            return None;
        }
        prev = threshold;
        let fees = FeesBps {
            lp: u64le(data, at + 16)?,
            protocol: u64le(data, at + 24)?,
            creator: u64le(data, at + 32)?,
        };
        out.push((threshold, fees));
        at += 40;
    }
    Some((out, at))
}

fn u64le(data: &[u8], off: usize) -> Option<u64> {
    data.get(off..off + 8)
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .map(u64::from_le_bytes)
}

fn u32le(data: &[u8], off: usize) -> Option<u32> {
    data.get(off..off + 4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_fee_bytes() -> Vec<u8> {
        include_bytes!("../../listener/tests/fixtures/fee_config_live.bin").to_vec()
    }

    #[test]
    fn live_schedule_resolves_standard_tier() {
        let sched = parse_schedule(&live_fee_bytes()).expect("live decodes");
        assert_eq!(select_tier(&sched, QuoteKind::SolLike, 0), Some((95, 30)));
        assert_eq!(
            select_tier(&sched, QuoteKind::SolLike, u128::MAX),
            Some((95, 30))
        );
        assert_eq!(select_tier(&sched, QuoteKind::Usdc, 0), Some((95, 30)));
        assert_eq!(select_tier(&sched, QuoteKind::Other, 0), Some((95, 30)));
    }

    fn two_tier_bytes() -> Vec<u8> {
        let mut v = vec![0u8; 8 + 1 + 32];
        v.extend_from_slice(&0u64.to_le_bytes()); // flat lp
        v.extend_from_slice(&95u64.to_le_bytes()); // flat proto
        v.extend_from_slice(&30u64.to_le_bytes()); // flat creator
        v.extend_from_slice(&2u32.to_le_bytes()); // tiers: 2 rows
        v.extend_from_slice(&0u128.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend_from_slice(&95u64.to_le_bytes());
        v.extend_from_slice(&30u64.to_le_bytes());
        v.extend_from_slice(&1_000_000u128.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend_from_slice(&80u64.to_le_bytes());
        v.extend_from_slice(&20u64.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // stable: 1 row
        v.extend_from_slice(&0u128.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend_from_slice(&95u64.to_le_bytes());
        v.extend_from_slice(&30u64.to_le_bytes());
        v
    }

    #[test]
    fn multi_tier_selects_by_market_cap() {
        let sched = parse_schedule(&two_tier_bytes()).expect("two-tier decodes");
        assert_eq!(
            select_tier(&sched, QuoteKind::SolLike, 999_999),
            Some((95, 30))
        );
        assert_eq!(
            select_tier(&sched, QuoteKind::SolLike, 1_000_000),
            Some((80, 20))
        );
        assert_eq!(select_tier(&sched, QuoteKind::SolLike, 0), Some((95, 30)));
        let empty = FeeSchedule {
            tiers: vec![],
            stable: vec![],
            flat: FeesBps { lp: 0, protocol: 95, creator: 30 },
            exotic: FeesBps::default(),
        };
        assert_eq!(select_tier(&empty, QuoteKind::SolLike, 0), None);
        assert_eq!(parse_schedule(&two_tier_bytes()[..72]), None);
    }

    #[test]
    fn descending_tiers_fail_closed() {
        // Thresholds must ascend; anything else refuses (tier
        // selection would mis-pick silently). Row 2 lives at 109..125.
        let mut bytes = two_tier_bytes();
        bytes[109..125].copy_from_slice(&0u128.to_le_bytes());
        let sched = parse_schedule(&bytes).expect("shape still decodes");
        // Equal thresholds resolve to the last row (documented
        // tie-break: highest threshold <= mcap, later wins ties).
        assert_eq!(select_tier(&sched, QuoteKind::SolLike, 0), Some((80, 20)));
        let mut bad = two_tier_bytes();
        bad[69..85].copy_from_slice(&5u128.to_le_bytes());
        bad[109..125].copy_from_slice(&0u128.to_le_bytes());
        assert_eq!(parse_schedule(&bad), None);
    }

    #[test]
    fn market_cap_math_fails_closed() {
        assert_eq!(
            market_cap(30_000_000_000, 1_073_000_000_000_000, 1_000_000_000_000_000),
            {
                let m = 30_000_000_000u128 * 1_000_000_000_000_000u128 / 1_073_000_000_000_000u128;
                Some(m)
            }
        );
        assert_eq!(market_cap(1, 0, 1), None);
    }

    #[test]
    fn curve_bytes_yield_view() {
        let mut data = vec![0u8; 123];
        data[8..16].copy_from_slice(&1_000u64.to_le_bytes()); // vt
        data[16..24].copy_from_slice(&2_000u64.to_le_bytes()); // vq
        data[40..48].copy_from_slice(&3_000u64.to_le_bytes()); // stored supply
        data[115..123].copy_from_slice(&250u64.to_le_bytes()); // override
        let view = curve_fee_inputs(&data).expect("view");
        assert_eq!(view.kind, QuoteKind::SolLike);
        assert_eq!(view.coin_override, 250);
        assert_eq!(
            (view.vt, view.vq, view.stored_supply),
            (1_000, 2_000, 3_000)
        );
        assert!(!view.is_mayhem);
        assert!(view.creator_is_default);
        data[ids::CURVE_CREATOR_OFFSET..ids::CURVE_CREATOR_END].copy_from_slice(&[9u8; 32]);
        assert!(!curve_fee_inputs(&data).expect("view").creator_is_default);
        data[83..115].copy_from_slice(&ids::WSOL_MINT.to_bytes());
        assert_eq!(curve_fee_inputs(&data).expect("w").kind, QuoteKind::SolLike);
        data[83..115].copy_from_slice(&ids::USDC_MINT.to_bytes());
        assert_eq!(curve_fee_inputs(&data).expect("u").kind, QuoteKind::Usdc);
        data[83..115].copy_from_slice(&[7u8; 32]);
        assert_eq!(curve_fee_inputs(&data).expect("e").kind, QuoteKind::Other);
        data[ids::CURVE_MAYHEM_FLAG_OFFSET] = 1;
        assert!(curve_fee_inputs(&data).expect("m").is_mayhem);
        assert_eq!(curve_fee_inputs(&data[..122]), None);
    }

    #[test]
    fn override_applies_before_total() {
        // The real rule (not a local copy): gate-on override wins,
        // creatorless curves charge no creator fee at all.
        assert_eq!(effective_creator_bps(250, 30, true, false), 250);
        assert_eq!(effective_creator_bps(250, 30, false, false), 30);
        assert_eq!(effective_creator_bps(0, 30, true, false), 30);
        assert_eq!(effective_creator_bps(250, 30, true, true), 0);
        assert_eq!(effective_creator_bps(0, 0, true, true), 0);
        assert_eq!(
            effective_total(95, effective_creator_bps(250, 30, true, false)),
            Some(345)
        );
        assert_eq!(
            effective_total(95, effective_creator_bps(250, 30, false, false)),
            Some(125)
        );
        assert_eq!(
            effective_total(95, effective_creator_bps(0, 30, true, false)),
            Some(125)
        );
        assert_eq!(effective_total(u64::MAX, 1), None);
        assert_eq!(effective_total(9_000, 9_000), None);
    }

    #[test]
    fn creatorless_curve_zeroes_creator_leg() {
        // End-to-end over curve bytes: default creator key -> proto only,
        // override or not (program rule shared with dispatch).
        let bytes = |creator: [u8; 32], override_bps: u64| {
            let mut data = vec![0u8; 123];
            data[8..16].copy_from_slice(&1_000u64.to_le_bytes());
            data[16..24].copy_from_slice(&2_000u64.to_le_bytes());
            data[40..48].copy_from_slice(&3_000u64.to_le_bytes());
            data[49..81].copy_from_slice(&creator);
            data[115..123].copy_from_slice(&override_bps.to_le_bytes());
            data
        };
        let sched = parse_schedule(&two_tier_bytes()).expect("two-tier decodes");
        let total = |creator: [u8; 32], override_bps: u64, gate_on: bool| {
            let view = curve_fee_inputs(&bytes(creator, override_bps)).expect("view");
            let (proto, creator) = select_tier(
                &sched,
                view.kind,
                market_cap(view.vq, view.vt, view.stored_supply).expect("mcap"),
            )
            .expect("tier");
            let coin = effective_creator_bps(
                view.coin_override,
                creator,
                gate_on,
                view.creator_is_default,
            );
            effective_total(proto, coin).expect("total")
        };
        // Live tier at mcap 2*3000/1000 = 6000 lamports is (95, 30).
        assert_eq!(total([9u8; 32], 0, true), 125);
        assert_eq!(total([9u8; 32], 250, true), 345);
        assert_eq!(total([0u8; 32], 250, true), 95);
        assert_eq!(total([0u8; 32], 250, false), 95);
    }
}
