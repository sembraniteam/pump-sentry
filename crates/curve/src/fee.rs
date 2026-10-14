//! On-chain venue-fee read for the paper ledger.
//!
//! [`venue_fee_bps`] resolves a curve's total fee (protocol + coin
//! creator, bps/side) from live chain state: the curve account (quote
//! kind + the coin's own creator rate), the FeeConfig schedule, and the
//! Global configurability gate. The confirm path registers the result
//! per mint ([`simulator::PaperPortfolio::set_venue_fee_for_mint`]) so
//! custom-fee coins price exactly instead of at the standard 125 bps.
//!
//! Single-tier fast path, deliberately: only one-entry tier tables are
//! resolved (today's live schedule is a single `(0, 95, 30)` tier
//! everywhere). A multi-tier schedule returns `None` — the paper leg
//! keeps the standard tier while the executor's full fee engine
//! (`executor::quote`) remains the source of truth for dispatch. Any
//! transport/shape failure is also `None` (fail-closed to the default,
//! never a zeroed cost).

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Deadline for the joined fee-input reads (confirm path only).
pub const FEE_TIMEOUT: Duration = Duration::from_secs(10);
/// FeeConfig/Global cache TTL: program-owned globals change only on
/// upgrades, so one joined read per hour bounds confirm-path RPCs
/// (the per-curve account is still read fresh every call — the coin's
/// own rate lives there).
pub const FEE_CACHE_TTL: Duration = Duration::from_secs(3600);

/// Cached global fee inputs: raw FeeConfig bytes, the Global gate byte,
/// and the fetch time. Raw bytes (not parsed triples) so a future
/// schedule shape re-parses instead of serving a stale interpretation.
pub type FeeInputsCache = Arc<Mutex<Option<(Vec<u8>, u8, Instant)>>>;

/// Quote-mint class selecting the fee table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteKind {
    SolLike,
    Usdc,
    Other,
}

/// Total venue fee (protocol + coin creator, bps/side) for one curve.
/// `None` on any transport/shape/multi-tier failure — callers keep the
/// standard tier.
pub async fn venue_fee_bps(rpc: &RpcClient, cache: &FeeInputsCache, curve: &Pubkey) -> Option<f64> {
    let curve_data = tokio::time::timeout(FEE_TIMEOUT, rpc.get_account(curve))
        .await
        .ok()?
        .ok()?;
    if curve_data.owner != ids::PUMP_PROGRAM_ID {
        return None;
    }
    let (kind, coin_override) = curve_fee_inputs(&curve_data.data)?;
    let (fee_bytes, gate_byte) = cached_fee_inputs(rpc, cache).await?;
    let gate_on = match gate_byte {
        0 => false,
        1 => true,
        _ => return None,
    };
    let (proto, creator) = first_tier_for(&fee_bytes, kind)?;
    Some(effective_total(proto, creator, gate_on, coin_override)? as f64)
}

/// Combine schedule rates with the coin override (engine rule): the
/// coin's own rate wins while the gate is on and nonzero, otherwise the
/// schedule rate stands. Pure; `None` past 100% (fail-closed, never a
/// jumbo fee).
fn effective_total(proto: u64, creator: u64, gate_on: bool, coin_override: u64) -> Option<u64> {
    let creator = if gate_on && coin_override != 0 {
        coin_override
    } else {
        creator
    };
    let total = proto.checked_add(creator)?;
    if total > 10_000 {
        return None;
    }
    Some(total)
}

/// Joined FeeConfig + Global read behind the TTL cache. Owner-verified
/// (FeeConfig under the fee program, Global under Pump); shape is NOT
/// validated here — parsing re-runs per call so a schedule upgrade
/// fails closed instead of serving a cached misread.
async fn cached_fee_inputs(rpc: &RpcClient, cache: &FeeInputsCache) -> Option<(Vec<u8>, u8)> {
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
    let gate = *global.data.get(ids::GLOBAL_FEE_GATE_OFFSET)?;
    *cache.lock().await = Some((fee.data.clone(), gate, Instant::now()));
    Some((fee.data, gate))
}

/// Quote kind + the coin's own creator rate from curve bytes. Pure;
/// `None` on short data.
fn curve_fee_inputs(data: &[u8]) -> Option<(QuoteKind, u64)> {
    if data.len() < ids::CURVE_COIN_CREATOR_FEE_END {
        return None;
    }
    let raw: [u8; 32] = data
        .get(ids::CURVE_QUOTE_MINT_OFFSET..ids::CURVE_QUOTE_MINT_END)?
        .try_into()
        .ok()?;
    let quote = Pubkey::new_from_array(raw);
    let kind = if quote == Pubkey::default() {
        QuoteKind::SolLike
    } else if quote == ids::USDC_MINT {
        QuoteKind::Usdc
    } else {
        QuoteKind::Other
    };
    let coin = u64le(data, ids::CURVE_COIN_CREATOR_FEE_OFFSET)?;
    Some((kind, coin))
}

/// First-tier (protocol, creator) triple for one quote kind. Pure.
/// Every consulted table must hold exactly one tier — the single-tier
/// fast path; anything else is `None` (fail-closed to the default).
fn first_tier_for(data: &[u8], kind: QuoteKind) -> Option<(u64, u64)> {
    // Layout mirror of the executor's FeeConfig decoder (disc 8 +
    // bump 1 + admin 32 + flat triple 24): tiers vec at 65, then the
    // stable vec, then the exotic triple (zeros = unset → flat).
    const TIERS_OFF: usize = 8 + 1 + 32 + 24;
    let (tiers, at) = tier_triple(data, TIERS_OFF)?;
    let (stable, at) = tier_triple(data, at)?;
    let exotic = if data.len() >= at + 24 {
        (u64le(data, at + 8)?, u64le(data, at + 16)?)
    } else if data.len() == at {
        (0, 0)
    } else {
        return None;
    };
    let flat = (u64le(data, 49)?, u64le(data, 57)?);
    match kind {
        QuoteKind::SolLike => Some(tiers),
        QuoteKind::Usdc => Some(stable),
        QuoteKind::Other => {
            if exotic == (0, 0) {
                Some(flat)
            } else {
                Some(exotic)
            }
        }
    }
}

/// One single-entry tier table at `off`: the (protocol, creator) triple
/// plus the next offset. `None` unless the table holds exactly one tier.
fn tier_triple(data: &[u8], off: usize) -> Option<((u64, u64), usize)> {
    if u32le(data, off)? != 1 {
        return None;
    }
    let proto = u64le(data, off + 4 + 24)?;
    let creator = u64le(data, off + 4 + 32)?;
    Some(((proto, creator), off + 4 + 40))
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
        // Live FeeConfig (single (0, 95, 30) tier everywhere): SOL and
        // USDC both price 95 + 30.
        let bytes = live_fee_bytes();
        assert_eq!(first_tier_for(&bytes, QuoteKind::SolLike), Some((95, 30)));
        assert_eq!(first_tier_for(&bytes, QuoteKind::Usdc), Some((95, 30)));
        assert_eq!(first_tier_for(&bytes, QuoteKind::Other), Some((95, 30)));
    }

    #[test]
    fn multi_tier_fails_closed() {
        // A second tier anywhere (tiers or stable) refuses: the paper
        // leg must not guess tier selection without market-cap inputs.
        let mut bytes = live_fee_bytes();
        bytes[65..69].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(first_tier_for(&bytes, QuoteKind::SolLike), None);
        let mut bytes = live_fee_bytes();
        // Stable vec offset for the live single-tier file: 65 + 4 + 40.
        bytes[109..113].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(first_tier_for(&bytes, QuoteKind::Usdc), None);
        // Short garbage fails closed too.
        assert_eq!(first_tier_for(&bytes[..72], QuoteKind::SolLike), None);
    }

    #[test]
    fn curve_bytes_yield_kind_and_override() {
        let mut data = vec![0u8; 123];
        // Zero quote = SOL-paired; override 250.
        data[115..123].copy_from_slice(&250u64.to_le_bytes());
        assert_eq!(curve_fee_inputs(&data), Some((QuoteKind::SolLike, 250)));
        // USDC quote routes to the stable table.
        data[83..115].copy_from_slice(&ids::USDC_MINT.to_bytes());
        assert_eq!(curve_fee_inputs(&data), Some((QuoteKind::Usdc, 250)));
        // Exotic quote.
        data[83..115].copy_from_slice(&[7u8; 32]);
        assert_eq!(curve_fee_inputs(&data), Some((QuoteKind::Other, 250)));
        // Short data fails closed.
        assert_eq!(curve_fee_inputs(&data[..122]), None);
    }

    #[test]
    fn override_wins_only_behind_the_gate() {
        // Engine rule, pinned: gate on + nonzero override wins (95 + 250
        // = 345 observed live); gate off or zero override keeps 125.
        assert_eq!(effective_total(95, 30, true, 250), Some(345));
        assert_eq!(effective_total(95, 30, false, 250), Some(125));
        assert_eq!(effective_total(95, 30, true, 0), Some(125));
        // Absurd totals fail closed, never a jumbo fee.
        assert_eq!(effective_total(u64::MAX, 1, true, 0), None);
        assert_eq!(effective_total(9_000, 9_000, true, 0), None);
    }
}
