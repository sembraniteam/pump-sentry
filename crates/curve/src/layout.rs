//! Bonding-curve account layout.
//! Little-endian: disc 0..8, u64 reserves at 8/16/24/32/40, flag at 48.

use solana_sdk::pubkey::Pubkey;
use thiserror::Error;

/// Re-exported single source (see `ids::BONDING_CURVE_DISCRIMINATOR`).
pub use ids::BONDING_CURVE_DISCRIMINATOR;

/// Min data length: 8 + 5x8 + 1.
pub const BONDING_CURVE_MIN_LEN: usize = 49;

/// Offset of `token_total_supply`.
pub const TOKEN_SUPPLY_OFFSET: usize = 40;
/// Length of `token_total_supply`.
pub const TOKEN_SUPPLY_LEN: usize = 8;

/// Offset of the `complete` flag.
pub const COMPLETE_FLAG_OFFSET: usize = 48;

// Pinned to the canonical offset in `ids`.
const _: () = assert!(COMPLETE_FLAG_OFFSET == ids::CURVE_COMPLETE_OFFSET);

/// Pump.fun mint decimals.
pub const PUMP_TOKEN_DECIMALS: u8 = 6;

/// Lamports per SOL.
pub const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

/// Integer lamports per SOL for [`BondingCurveReserves::price_ratio_u128`].
/// Funds move in u128 lamports/base-units; the f64 above is display-only.
const LAMPORTS_PER_SOL_U128: u128 = 1_000_000_000;

/// Reserve parse failures.
#[derive(Error, Debug, PartialEq)]
pub enum CurveParseError {
    #[error("bonding curve data too short: {0} bytes")]
    DataTooShort(usize),
    #[error("bonding curve discriminator mismatch")]
    BadDiscriminator,
    #[error("bonding curve account owned by {0}, not the pump program")]
    WrongOwner(String),
}

/// Parsed reserves of a Pump.fun curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BondingCurveReserves {
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub token_total_supply: u64,
    /// True once migrated (spot is stale).
    pub complete: bool,
}

impl BondingCurveReserves {
    /// Parse raw account data.
    pub fn parse(data: &[u8]) -> Result<Self, CurveParseError> {
        if data.len() < BONDING_CURVE_MIN_LEN {
            return Err(CurveParseError::DataTooShort(data.len()));
        }
        if data[..8] != BONDING_CURVE_DISCRIMINATOR {
            return Err(CurveParseError::BadDiscriminator);
        }
        // `.get()` even behind the length gate: offsets and the gate
        // must never drift apart silently.
        let short = || CurveParseError::DataTooShort(data.len());
        let u64le = |at: usize| -> Result<u64, CurveParseError> {
            data.get(at..at + 8)
                .and_then(|b| <[u8; 8]>::try_from(b).ok())
                .map(u64::from_le_bytes)
                .ok_or_else(short)
        };
        Ok(Self {
            virtual_token_reserves: u64le(8)?,
            virtual_sol_reserves: u64le(16)?,
            real_token_reserves: u64le(24)?,
            real_sol_reserves: u64le(32)?,
            token_total_supply: u64le(40)?,
            complete: data.get(COMPLETE_FLAG_OFFSET).copied().unwrap_or(0) != 0,
        })
    }

    /// Parse with owner check.
    /// Prefer over [`Self::parse`] when the owner is known.
    pub fn parse_validated(data: &[u8], owner: &Pubkey) -> Result<Self, CurveParseError> {
        if owner != &ids::PUMP_PROGRAM_ID {
            return Err(CurveParseError::WrongOwner(owner.to_string()));
        }
        Self::parse(data)
    }

    /// Spot price, SOL per whole token.
    /// Display-only f64: funds-moving paths must use integer math
    /// ([`Self::price_ratio_u128`] or `executor::quote`) and never
    /// branch on this float.
    pub fn price_sol(&self, decimals: u8) -> Option<f64> {
        if self.virtual_token_reserves == 0 {
            return None;
        }
        let scale = 10f64.powi(i32::from(decimals));
        let price = self.virtual_sol_reserves as f64 / self.virtual_token_reserves as f64 * scale
            / LAMPORTS_PER_SOL;
        if price.is_finite() && price > 0.0 {
            Some(price)
        } else {
            None
        }
    }

    /// Integer price ratio: SOL per whole token equals
    /// `numer / denom` (both u128, no float). Funds-moving code uses
    /// this shape (`mul_div`-style) while [`Self::price_sol`] stays
    /// display-only. `None` on zero reserves or overflow.
    pub fn price_ratio_u128(&self, decimals: u8) -> Option<(u128, u128)> {
        if self.virtual_token_reserves == 0 {
            return None;
        }
        let scale = 10u128.checked_pow(u32::from(decimals))?;
        let numer = u128::from(self.virtual_sol_reserves).checked_mul(scale)?;
        let denom =
            u128::from(self.virtual_token_reserves).checked_mul(LAMPORTS_PER_SOL_U128)?;
        if denom == 0 {
            return None;
        }
        Some((numer, denom))
    }

    /// Spot price in USD per whole token.
    /// Display-only (see [`Self::price_sol`]): sizing/quotes stay integer.
    pub fn price_usd(&self, decimals: u8, sol_usd: f64) -> Option<f64> {
        self.price_sol(decimals).and_then(|p| {
            let usd = p * sol_usd;
            if usd.is_finite() && usd > 0.0 {
                Some(usd)
            } else {
                None
            }
        })
    }

    /// Liquidity proxy in USD (SOL side).
    /// Display-only f64 for gates/metrics; dispatch sizing is integer
    /// (`executor::quote` u128 math).
    pub fn liquidity_usd(&self, sol_usd: f64) -> f64 {
        let liq = self.virtual_sol_reserves as f64 / LAMPORTS_PER_SOL * sol_usd;
        if liq.is_finite() && liq > 0.0 {
            liq
        } else {
            0.0
        }
    }

    /// Fully-diluted market cap in USD. Test-only: no production caller
    /// uses it (pricing flows through `price_usd` + supply math inline).
    #[cfg(test)]
    pub fn market_cap_usd(&self, decimals: u8, sol_usd: f64) -> Option<f64> {
        self.price_usd(decimals, sol_usd).and_then(|p| {
            let scale = 10f64.powi(i32::from(decimals));
            let mcap = self.token_total_supply as f64 / scale * p;
            if mcap.is_finite() && mcap >= 0.0 {
                Some(mcap)
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn genesis_data() -> Vec<u8> {
        let mut data = Vec::with_capacity(49);
        data.extend_from_slice(&BONDING_CURVE_DISCRIMINATOR);
        for v in [
            1_073_000_000_000_000u64,
            30_000_000_000,
            1_073_000_000_000_000,
            0,
            1_000_000_000_000_000,
        ] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        data.push(0);
        data
    }

    #[test]
    fn parses_genesis_reserves() {
        let r = BondingCurveReserves::parse(&genesis_data()).expect("parse");
        assert_eq!(r.virtual_token_reserves, 1_073_000_000_000_000);
        assert_eq!(r.virtual_sol_reserves, 30_000_000_000);
        assert!(!r.complete);
    }

    #[test]
    fn rejects_short_and_foreign_data() {        assert_eq!(
            BondingCurveReserves::parse(&[0u8; 10]),
            Err(CurveParseError::DataTooShort(10))
        );
        let mut bad = genesis_data();
        bad[0] ^= 0xff;
        assert_eq!(
            BondingCurveReserves::parse(&bad),
            Err(CurveParseError::BadDiscriminator)
        );
    }

    #[test]
    fn genesis_spot_price() {
        let r = BondingCurveReserves::parse(&genesis_data()).expect("parse");
        let sol = r.price_sol(PUMP_TOKEN_DECIMALS).expect("price");
        assert!((sol - 2.7959e-8).abs() < 1e-12, "sol={sol}");
        let usd = r.price_usd(PUMP_TOKEN_DECIMALS, 150.0).expect("usd");
        assert!((usd - 2.7959e-8 * 150.0).abs() < 1e-12, "usd={usd}");
        assert!((r.liquidity_usd(150.0) - 4500.0).abs() < 1e-9);
        let mcap = r.market_cap_usd(PUMP_TOKEN_DECIMALS, 150.0).expect("mcap");
        assert!((mcap - 1e9 * usd).abs() < 1e-3, "mcap={mcap}");
        // Integer ratio agrees with the display float (funds paths use
        // the ratio, never the float).
        let (numer, denom) = r.price_ratio_u128(PUMP_TOKEN_DECIMALS).expect("ratio");
        assert_eq!(numer, 30_000_000_000u128 * 1_000_000u128);
        assert_eq!(denom, 1_073_000_000_000_000u128 * 1_000_000_000u128);
        let ratio = numer as f64 / denom as f64;
        assert!((ratio - sol).abs() < 1e-18, "ratio={ratio} sol={sol}");
    }

    #[test]
    fn empty_reserves_yield_no_price() {
        let mut r = BondingCurveReserves::parse(&genesis_data()).expect("parse");
        r.virtual_token_reserves = 0;
        assert_eq!(r.price_sol(6), None);
        assert_eq!(r.price_usd(6, 150.0), None);
    }

    #[test]
    fn validated_parse_gates_owner() {
        let data = genesis_data();
        assert!(BondingCurveReserves::parse_validated(&data, &ids::PUMP_PROGRAM_ID).is_ok());
        let stranger = Pubkey::new_from_array([9; 32]);
        assert_eq!(
            BondingCurveReserves::parse_validated(&data, &stranger),
            Err(CurveParseError::WrongOwner(stranger.to_string()))
        );
    }
}
