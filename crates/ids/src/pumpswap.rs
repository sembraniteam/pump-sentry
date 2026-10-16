//! PumpSwap AMM program addresses, discriminators, pool layout.
//!
//! Sources: pump-public-docs `PUMP_SWAP_README.md`,
//! `PUMP_SWAP_TRADE_V2.md`, `idl/pump_amm.json`. Pool layout proven
//! against mainnet pool `GeWJCTs9HW5KP7N4mkovNgRjCEszntTb7czmHtXbRrCK`
//! (owner, discriminator, every slot cross-checked with the
//! `migrate_v2` transaction that created it).

use solana_pubkey::Pubkey;

/// PumpSwap AMM program.
pub const PUMP_AMM_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
/// Program ID as string, for RPC filters. Test pins drift.
pub const PUMP_AMM_PROGRAM_ID_STR: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
/// Sole `GlobalConfig` account (PDA `["global_config"]`).
pub const PUMP_AMM_GLOBAL_CONFIG: Pubkey =
    Pubkey::from_str_const("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw");
/// Pump Fees `fee_config` PDA (`["fee_config", pump_amm_program_id]`).
pub const PUMP_AMM_FEE_CONFIG: Pubkey =
    Pubkey::from_str_const("5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx");
/// Seed for the CPI event-authority PDA.
pub const PUMP_AMM_EVENT_AUTHORITY_SEED: &[u8] = b"__event_authority";
/// Mayhem fixed supply for fee-tier market caps (per SDK).
pub const PUMP_AMM_MAYHEM_SUPPLY: u64 = 1_000_000_000_000_000;
/// Admin disable-flags byte (any nonzero refuses: unknown admin state).
pub const PUMP_AMM_GLOBAL_DISABLE_FLAGS_OFFSET: usize = 56;
/// `creator_fee_configurable` byte in GlobalConfig (proven live: 1).
pub const PUMP_AMM_GLOBAL_CREATOR_GATE_OFFSET: usize = 940;
/// GlobalConfig account length.
pub const PUMP_AMM_GLOBAL_LEN: usize = 949;
/// `sell_v2` discriminator: 17-account PumpSwap sells, fees stay in
/// the pool. `sha256("global:sell_v2")[..8]` reproduces these bytes.
/// Intentional discriminator collision with
/// `ids::PUMP_SELL_V2_DISCRIMINATOR` (same 8 bytes): never merge —
/// the two are disambiguated by program-id (PumpSwap AMM vs Pump).
/// See the cross-crate const-assert in `ids::lib`.
pub const PUMP_AMM_SELL_V2_DISCRIMINATOR: [u8; 8] = [93, 246, 130, 60, 231, 233, 64, 178];
/// `Pool` account discriminator (`sha256("account:Pool")[..8]`).
pub const PUMP_AMM_POOL_DISCRIMINATOR: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];

/// Pool bump byte.
pub const POOL_BUMP_OFFSET: usize = 8;
/// Pool index u16 (`0` on migration pools).
pub const POOL_INDEX_OFFSET: usize = 9;
/// Pool creator slot (pool authority on migrations).
pub const POOL_CREATOR_OFFSET: usize = 11;
/// Base mint slot (migrated token).
pub const POOL_BASE_MINT_OFFSET: usize = 43;
/// Quote mint slot (WSOL on executable pools).
pub const POOL_QUOTE_MINT_OFFSET: usize = 75;
/// LP mint slot end (exclusive).
pub const POOL_LP_MINT_END: usize = 139;
/// Base vault slot (token inventory).
pub const POOL_BASE_VAULT_OFFSET: usize = 139;
/// Quote vault slot (WSOL inventory).
pub const POOL_QUOTE_VAULT_OFFSET: usize = 171;
/// LP supply u64.
pub const POOL_LP_SUPPLY_OFFSET: usize = 203;
/// Coin creator slot.
pub const POOL_COIN_CREATOR_OFFSET: usize = 211;
/// Mayhem flag byte.
pub const POOL_MAYHEM_FLAG_OFFSET: usize = 243;
/// Cashback flag byte (v2 sells refuse these pools: 6079).
pub const POOL_CASHBACK_FLAG_OFFSET: usize = 244;
/// Virtual quote reserves i128 (signed; may be negative).
pub const POOL_VIRT_Q_OFFSET: usize = 245;
/// Virtual reserves slot end (exclusive).
pub const POOL_VIRT_Q_END: usize = 261;
/// Pool creator-fee rate u64le (0 = standard schedule).
pub const POOL_CREATOR_FEE_BPS_OFFSET: usize = 261;
/// Reserved flag byte (always false).
pub const POOL_CAN_EDIT_CREATOR_FEE_OFFSET: usize = 269;
/// Holder-rewards flag byte (trading unchanged).
pub const POOL_HOLDER_REWARD_FLAG_OFFSET: usize = 270;
/// Kept protocol-fee counter u64le.
pub const POOL_PROTOCOL_FEES_OFFSET: usize = 271;
/// Kept creator-fee counter u64le.
pub const POOL_CREATOR_FEES_OFFSET: usize = 279;
/// Full IDL layout length.
pub const POOL_FULL_LEN: usize = 287;
/// Min length: everything through virtual reserves. Older pools end
/// here (fee counters read 0); trailing bytes past 287 are ignored.
pub const POOL_MIN_LEN: usize = POOL_VIRT_Q_END;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_match_canonical_strings() {
        assert_eq!(
            PUMP_AMM_PROGRAM_ID.to_string(),
            PUMP_AMM_PROGRAM_ID_STR
        );
        assert_eq!(
            PUMP_AMM_PROGRAM_ID.to_string(),
            "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA"
        );
        assert_eq!(
            PUMP_AMM_GLOBAL_CONFIG.to_string(),
            "ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw"
        );
        assert_eq!(
            PUMP_AMM_FEE_CONFIG.to_string(),
            "5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx"
        );
    }

    #[test]
    fn pool_slots_are_ordered_and_fit() {
        assert_eq!(POOL_BUMP_OFFSET + 1, POOL_INDEX_OFFSET);
        assert_eq!(POOL_INDEX_OFFSET + 2, POOL_CREATOR_OFFSET);
        assert_eq!(POOL_CREATOR_OFFSET + 32, POOL_BASE_MINT_OFFSET);
        assert_eq!(POOL_BASE_MINT_OFFSET + 32, POOL_QUOTE_MINT_OFFSET);
        assert_eq!(POOL_QUOTE_MINT_OFFSET + 32 + 32, POOL_BASE_VAULT_OFFSET);
        assert_eq!(POOL_BASE_VAULT_OFFSET + 32, POOL_QUOTE_VAULT_OFFSET);
        assert_eq!(POOL_QUOTE_VAULT_OFFSET + 32, POOL_LP_SUPPLY_OFFSET);
        assert_eq!(POOL_MAYHEM_FLAG_OFFSET + 1, POOL_CASHBACK_FLAG_OFFSET);
        assert_eq!(POOL_CASHBACK_FLAG_OFFSET + 1, POOL_VIRT_Q_OFFSET);
        assert_eq!(POOL_VIRT_Q_OFFSET + 16, POOL_CREATOR_FEE_BPS_OFFSET);
        assert_eq!(POOL_CREATOR_FEE_BPS_OFFSET + 8, POOL_CAN_EDIT_CREATOR_FEE_OFFSET);
        assert_eq!(POOL_CAN_EDIT_CREATOR_FEE_OFFSET + 1, POOL_HOLDER_REWARD_FLAG_OFFSET);
        assert_eq!(POOL_HOLDER_REWARD_FLAG_OFFSET + 1, POOL_PROTOCOL_FEES_OFFSET);
        assert_eq!(POOL_CREATOR_FEES_OFFSET + 8, POOL_FULL_LEN);
    }
}
