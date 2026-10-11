//! Bonding-curve account layout (Pump.fun program).
//!
//! Wire format, little-endian:
//! ```text
//! offset  size  field
//! 0       8     discriminator (sha256("account:BondingCurve")[..8])
//! 8       8     virtual_token_reserves (u64, base units)
//! 16      8     virtual_sol_reserves   (u64, lamports)
//! 24      8     real_token_reserves    (u64)
//! 32      8     real_sol_reserves      (u64)
//! 40      8     token_total_supply     (u64)
//! 48      1     complete               (bool)
//! 49      32    creator                (Pubkey, newer curves; ignored here)
//! ```
//!
//! Spot price follows the constant-product invariant: SOL per token =
//! `virtual_sol / virtual_token`, scaled by token decimals.

use solana_sdk::pubkey::Pubkey;
use thiserror::Error;

/// sha256(`"account:BondingCurve"`)[..8], verified by hashing.
pub const BONDING_CURVE_DISCRIMINATOR: [u8; 8] = [23, 183, 248, 55, 96, 216, 172, 96];

/// Minimum account data length: 8 (disc) + 5×8 (u64) + 1 (bool).
pub const BONDING_CURVE_MIN_LEN: usize = 49;

/// Byte offset of the `token_total_supply` u64 (single source of truth;
/// `migration` and `listener` must use this, never a literal range).
pub const TOKEN_SUPPLY_OFFSET: usize = 40;
/// Length of the `token_total_supply` u64 field.
pub const TOKEN_SUPPLY_LEN: usize = 8;

/// Byte offset of the `complete` graduation flag (single source of
/// truth; same rule as above).
pub const COMPLETE_FLAG_OFFSET: usize = 48;

/// Pump.fun mint decimals.
pub const PUMP_TOKEN_DECIMALS: u8 = 6;

/// Lamports per SOL.
pub const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

/// Reserve parse failures.
#[derive(Error, Debug, PartialEq)]
pub enum CurveParseError {
    /// Account data shorter than 49 bytes.
    #[error("bonding curve data too short: {0} bytes")]
    DataTooShort(usize),
    /// First 8 bytes are not the bonding-curve discriminator.
    #[error("bonding curve discriminator mismatch")]
    BadDiscriminator,
    /// Account owner is not the Pump.fun program (spoofed price source).
    #[error("bonding curve account owned by {0}, not the pump program")]
    WrongOwner(String),
}

/// Parsed virtual + real reserves of a Pump.fun bonding curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BondingCurveReserves {
    /// Virtual token reserves (base units) — AMM invariant side.
    pub virtual_token_reserves: u64,
    /// Virtual SOL reserves (lamports) — AMM invariant side.
    pub virtual_sol_reserves: u64,
    /// Real tokens still in the curve.
    pub real_token_reserves: u64,
    /// Real SOL collected (excludes migration threshold accounting).
    pub real_sol_reserves: u64,
    /// Total token supply (base units).
    pub token_total_supply: u64,
    /// True once the curve migrated (spot reads are stale afterwards).
    pub complete: bool,
}

impl BondingCurveReserves {
    /// Parse raw account data. Bounds-checked; never panics.
    pub fn parse(data: &[u8]) -> Result<Self, CurveParseError> {        if data.len() < BONDING_CURVE_MIN_LEN {
            return Err(CurveParseError::DataTooShort(data.len()));
        }
        if data[..8] != BONDING_CURVE_DISCRIMINATOR {
            return Err(CurveParseError::BadDiscriminator);
        }
        let u64le = |at: usize| -> u64 {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&data[at..at + 8]);
            u64::from_le_bytes(buf)
        };
        Ok(Self {
            virtual_token_reserves: u64le(8),
            virtual_sol_reserves: u64le(16),
            real_token_reserves: u64le(24),
            real_sol_reserves: u64le(32),
            token_total_supply: u64le(40),
            complete: data[48] != 0,
        })
    }

    /// Parse with program-ownership gating: any account with a colliding
    /// prefix but a foreign owner (spoofed price source) is rejected.
    /// Prefer this over [`Self::parse`] whenever the account owner is
    /// known (every RPC read returns it).
    pub fn parse_validated(data: &[u8], owner: &Pubkey) -> Result<Self, CurveParseError> {
        if owner != &ids::PUMP_PROGRAM_ID {
            return Err(CurveParseError::WrongOwner(owner.to_string()));
        }
        Self::parse(data)
    }

    /// Spot price in SOL per whole token, or `None` on empty reserves.
    /// f64 division keeps ~1e-16 relative precision at any magnitude,
    /// which is what a price ratio needs (absolute lamport exactness
    /// past 2^53 is irrelevant here).
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

    /// Spot price in USD per whole token.
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

    /// Liquidity proxy in USD: the SOL side of the virtual reserves.
    ///
    /// Documented approximation — the AMM's buy-side depth at spot.
    pub fn liquidity_usd(&self, sol_usd: f64) -> f64 {
        let liq = self.virtual_sol_reserves as f64 / LAMPORTS_PER_SOL * sol_usd;
        if liq.is_finite() && liq > 0.0 {
            liq
        } else {
            0.0
        }
    }

    /// Fully-diluted market cap in USD at spot.
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

    /// Genesis-like reserves: 1.073e9 tokens (6dp) vs 30 virtual SOL.
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
        // 30 SOL / 1.073e9 tokens ≈ 2.7959e-8 SOL.
        let sol = r.price_sol(PUMP_TOKEN_DECIMALS).expect("price");
        assert!((sol - 2.7959e-8).abs() < 1e-12, "sol={sol}");
        let usd = r.price_usd(PUMP_TOKEN_DECIMALS, 150.0).expect("usd");
        assert!((usd - 2.7959e-8 * 150.0).abs() < 1e-12, "usd={usd}");
        // Liquidity ≈ 30 SOL side.
        assert!((r.liquidity_usd(150.0) - 4500.0).abs() < 1e-9);
        // FDV ≈ 1e9 tokens × spot.
        let mcap = r.market_cap_usd(PUMP_TOKEN_DECIMALS, 150.0).expect("mcap");
        assert!((mcap - 1e9 * usd).abs() < 1e-3, "mcap={mcap}");
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
