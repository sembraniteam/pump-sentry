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
/// must reach paper within minutes, and a cold read is only 2 RPCs on
/// the rare confirm path.
pub const FEE_CACHE_TTL: Duration = Duration::from_secs(900);

/// Cached globals: FeeConfig bytes, gate byte, fetch time.
pub type FeeInputsCache = Arc<Mutex<Option<(Vec<u8>, u8, Instant)>>>;

/// Mint supply cache. Supplies never change on bonding curves (fixed at
/// creation; gated mints have no live mint authority), so entries never
/// expire — same bound discipline as the decimals cache.
pub type SupplyCache = Arc<Mutex<HashMap<String, u64>>>;

/// Quote-mint class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteKind {
    SolLike,
    Usdc,
    Other,
}

/// Total venue fee (bps/side) for one curve.
pub async fn venue_fee_bps(
    rpc: &RpcClient,
    cache: &FeeInputsCache,
    supply_cache: &SupplyCache,
    mint: &Pubkey,
    curve: &Pubkey,
) -> Option<f64> {
    let curve_data = tokio::time::timeout(FEE_TIMEOUT, rpc.get_account(curve))
        .await
        .ok()?
        .ok()?;
    venue_fee_for_data(
        rpc,
        cache,
        supply_cache,
        mint,
        &curve_data.owner,
        &curve_data.data,
    )
    .await
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
        view.stored_supply
    } else {
        mint_supply(rpc, supply_cache, mint).await?
    };
    let (fee_bytes, gate_byte) = cached_fee_inputs(rpc, cache).await?;
    let gate_on = match gate_byte {
        0 => false,
        1 => true,
        _ => return None,
    };
    let schedule = parse_schedule(&fee_bytes)?;
    let (proto, creator) =
        select_tier(&schedule, view.kind, market_cap(view.vq, view.vt, supply)?)?;
    let coin = if gate_on && view.coin_override != 0 {
        view.coin_override
    } else {
        creator
    };
    Some(effective_total(proto, coin)? as f64)
}

/// Basis-point denominator (mirrors executor math; u64 domain here).
const BPS_DENOMINATOR: u64 = 10_000;

/// Venue total; `None` past 100%.
fn effective_total(proto: u64, creator: u64) -> Option<u64> {
    let total = proto.checked_add(creator)?;
    if total > BPS_DENOMINATOR {
        return None;
    }
    Some(total)
}

/// Joined FeeConfig + Global read behind the TTL cache.
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

/// Curve fee inputs for tier selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CurveFeeView {
    kind: QuoteKind,
    coin_override: u64,
    vt: u64,
    vq: u64,
    stored_supply: u64,
    is_mayhem: bool,
}

fn curve_fee_inputs(data: &[u8]) -> Option<CurveFeeView> {
    if data.len() < ids::CURVE_COIN_CREATOR_FEE_END {
        return None;
    }
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
    })
}

/// Mint supply for fee-tier market caps (cached: immutable on curves).
async fn mint_supply(rpc: &RpcClient, cache: &SupplyCache, mint: &Pubkey) -> Option<u64> {
    let key = mint.to_string();
    if let Some(hit) = cache.lock().await.get(&key) {
        return Some(*hit);
    }
    let supply = tokio::time::timeout(FEE_TIMEOUT, rpc.get_token_supply(mint))
        .await
        .ok()?
        .ok()?;
    let supply = supply.amount.parse::<u64>().ok()?;
    let mut guard = cache.lock().await;
    if guard.len() >= crate::ticker::DECIMALS_CACHE_CAP
        && let Some(oldest) = guard.keys().next().cloned()
    {
        guard.remove(&oldest);
    }
    guard.insert(key, supply);
    Some(supply)
}

/// Market cap, lamports: `vq * supply / vt`.
fn market_cap(vq: u64, vt: u64, supply: u64) -> Option<u128> {
    if vt == 0 {
        return None;
    }
    u128::from(vq)
        .checked_mul(u128::from(supply))?
        .checked_div(u128::from(vt))
}

/// One fee tier: threshold (lamports) + (protocol, creator).
type FeeTierRow = (u128, (u64, u64));

/// Full fee schedule, picked by market cap.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FeeSchedule {
    tiers: Vec<FeeTierRow>,
    stable: Vec<FeeTierRow>,
    flat: (u64, u64),
    exotic: (u64, u64),
}

/// Tier-table bound (DoS cap on hostile lengths; single source is
/// `ids::MAX_FEE_TIERS`).
use ids::MAX_FEE_TIERS;

/// Highest tier at or below mcap, else the first.
fn tier_for(tiers: &[FeeTierRow], mcap: u128) -> Option<(u64, u64)> {
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

/// (protocol, creator) for one quote kind at `mcap`.
fn select_tier(schedule: &FeeSchedule, kind: QuoteKind, mcap: u128) -> Option<(u64, u64)> {
    match kind {
        QuoteKind::SolLike => tier_for(&schedule.tiers, mcap),
        QuoteKind::Usdc => tier_for(&schedule.stable, mcap),
        QuoteKind::Other => {
            if schedule.exotic == (0, 0) {
                Some(schedule.flat)
            } else {
                Some(schedule.exotic)
            }
        }
    }
}

/// Decode the schedule from FeeConfig bytes.
fn parse_schedule(data: &[u8]) -> Option<FeeSchedule> {
    const TIERS_OFF: usize = 8 + 1 + 32 + 24;
    let flat = (u64le(data, 49)?, u64le(data, 57)?);
    let (tiers, at) = parse_tier_vec(data, TIERS_OFF)?;
    let (stable, at) = parse_tier_vec(data, at)?;
    let exotic = if data.len() >= at + 24 {
        (u64le(data, at + 8)?, u64le(data, at + 16)?)
    } else if data.len() == at {
        (0, 0)
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
        let proto = u64le(data, at + 24)?;
        let creator = u64le(data, at + 32)?;
        out.push((threshold, (proto, creator)));
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
            flat: (95, 30),
            exotic: (0, 0),
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
        let coin = |gate_on: bool, coin_override: u64, creator: u64| {
            if gate_on && coin_override != 0 {
                coin_override
            } else {
                creator
            }
        };
        assert_eq!(effective_total(95, coin(true, 250, 30)), Some(125 + 220));
        assert_eq!(effective_total(95, coin(false, 250, 30)), Some(125));
        assert_eq!(effective_total(95, coin(true, 0, 30)), Some(125));
        assert_eq!(effective_total(u64::MAX, 1), None);
        assert_eq!(effective_total(9_000, 9_000), None);
    }
}
