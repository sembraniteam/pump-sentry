//! Pump.fun program addresses and instruction discriminators.

use solana_pubkey::Pubkey;

/// Pump.fun program.
pub const PUMP_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
/// Program ID as string, for RPC filters. Test pins drift.
pub const PUMP_PROGRAM_ID_STR: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
/// Fee recipient (writable).
pub const FEE_RECIPIENT: Pubkey =
    Pubkey::from_str_const("CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM");
/// Global config PDA (readonly).
pub const GLOBAL_ACCOUNT: Pubkey =
    Pubkey::from_str_const("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf");
/// CPI event authority (readonly).
pub const EVENT_AUTHORITY: Pubkey =
    Pubkey::from_str_const("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1");
/// Authority signing Pump.fun migrations.
pub const PUMP_MIGRATION_AUTHORITY: Pubkey =
    Pubkey::from_str_const("39azUYFWPz3VHgKCf3VChUwbpURdCHRxjWVowf5jUJjg");

/// Bonding-curve account discriminator: sha256(`"account:BondingCurve"`)[..8].
/// Single source for every curve-state parser (paper ledger and dispatch
/// must agree on what a curve account is).
pub const BONDING_CURVE_DISCRIMINATOR: [u8; 8] = [23, 183, 248, 55, 96, 216, 172, 96];

/// `create` instruction discriminator (skill solana-geyser-pumpfun).
pub const PUMP_CREATE_DISCRIMINATOR: [u8; 8] = [24, 30, 200, 40, 5, 28, 7, 119];
/// `create_v2` discriminator (creator/mayhem fields, 14-account layout).
pub const PUMP_CREATE_V2_DISCRIMINATOR: [u8; 8] = [214, 144, 76, 236, 95, 139, 49, 180];
/// `buy` instruction discriminator (skill solana-pumpfun-swap).
pub const PUMP_BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
/// `sell` instruction discriminator (skill solana-pumpfun-swap).
pub const PUMP_SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
/// `buy_v2`: 27 accounts, 16-byte payload like legacy `buy`.
pub const PUMP_BUY_V2_DISCRIMINATOR: [u8; 8] = [184, 23, 238, 97, 103, 197, 211, 61];
/// `buy_v3`: 17 accounts. Fees stay on the curve.
/// `sha256("global:buy_v3")[..8]` reproduces these bytes.
pub const PUMP_BUY_V3_DISCRIMINATOR: [u8; 8] = [7, 5, 29, 196, 245, 23, 101, 80];
/// `sell_v3` discriminator: 17-account bonding-curve sells.
pub const PUMP_SELL_V3_DISCRIMINATOR: [u8; 8] = [28, 146, 222, 119, 38, 196, 105, 213];
/// `buy_exact_quote_in_v3` discriminator: exact-spend 17-account buys
/// (`spendable_quote_in`, `min_tokens_out`).
pub const PUMP_BUY_EXACT_QUOTE_IN_V3_DISCRIMINATOR: [u8; 8] =
    [225, 247, 80, 30, 213, 179, 132, 136];
/// `sell_v2`: 26 accounts, 24-byte payload like legacy `sell`.
/// Intentional discriminator collision with
/// `ids::PUMP_AMM_SELL_V2_DISCRIMINATOR` (same 8 bytes): never merge —
/// the two are disambiguated by program-id (Pump vs PumpSwap AMM).
/// See the cross-crate const-assert in `ids::lib`.
pub const PUMP_SELL_V2_DISCRIMINATOR: [u8; 8] = [93, 246, 130, 60, 231, 233, 64, 178];
/// `migrate` instruction discriminator (skill solana-raydium-migration).
pub const PUMP_MIGRATE_DISCRIMINATOR: [u8; 8] = [155, 234, 231, 146, 236, 158, 162, 30];
/// `migrate_v2` discriminator (pump-coin quotes, per-IDL account order).
pub const PUMP_MIGRATE_V2_DISCRIMINATOR: [u8; 8] = [187, 203, 18, 31, 206, 237, 254, 41];
/// Anchor CPI event discriminators (`sha256("event:<Name>")[..8]`).
/// Observed on mainnet in program logs AND `emit_cpi` self-CPI ixs
/// (both carry the same bytes): `TradeEvent` dozens per block,
/// `CompleteEvent` on the exhausting buy (frozen proof:
/// `complete_mayhem_buy_mainnet.json`, balance-reconciled).
/// `PostCompleteBuyEvent` is COMPUTED but UNOBSERVED (no pool-part
/// transaction found to date): the constant exists so a future
/// capture has a name to pin, but nothing parses it — decoding an
/// unseen layout would be fabrication.
pub const TRADE_EVENT_DISCRIMINATOR: [u8; 8] = [0xbd, 0xdb, 0x7f, 0xd3, 0x4e, 0xe6, 0x61, 0xee];
pub const COMPLETE_EVENT_DISCRIMINATOR: [u8; 8] = [0x5f, 0x72, 0x61, 0x9c, 0xd4, 0x2e, 0x98, 0x08];
pub const POST_COMPLETE_BUY_EVENT_DISCRIMINATOR: [u8; 8] =
    [0x6f, 0xb0, 0x6d, 0x8b, 0x31, 0x6c, 0xd5, 0xfb];
/// Pool account index in `migrate` (per pump IDL).
pub const MIGRATE_POOL_ACCOUNT_INDEX: usize = 9;
/// Pool account index in `migrate_v2` (per pump IDL).
pub const MIGRATE_V2_POOL_ACCOUNT_INDEX: usize = 10;
/// Fee config account: swap fee schedule (position 14).
pub const FEE_CONFIG_ACCOUNT: Pubkey =
    Pubkey::from_str_const("8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt");
/// Fee config program: fee routing (position 15).
pub const FEE_CONFIG_PROGRAM: Pubkey =
    Pubkey::from_str_const("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
/// Buyback recipients: 8 published wallets. Any one works on every coin
/// (v3 fees stay on the curve). Default is index 0.
pub const BUYBACK_FEE_RECIPIENTS: [Pubkey; 8] = [
    Pubkey::from_str_const("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD"),
    Pubkey::from_str_const("9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7"),
    Pubkey::from_str_const("GXPFM2caqTtQYC2cJ5yJRi9VDkpsYZXzYdwYpGnLmtDL"),
    Pubkey::from_str_const("3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR"),
    Pubkey::from_str_const("5cjcW9wExnJJiqgLjq7DEG75Pm6JBgE1hNv4B2vHXUW6"),
    Pubkey::from_str_const("EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL"),
    Pubkey::from_str_const("5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD"),
    Pubkey::from_str_const("A7hAgCzFw14fejgCp387JUJRMNyz4j89JKnhtKU8piqW"),
];
/// Default buyback pick (index 0).
pub const DEFAULT_BUYBACK_FEE_RECIPIENT: Pubkey = BUYBACK_FEE_RECIPIENTS[0];
/// USDC mint: `stable_fee_tiers`; SOL-like: `fee_tiers`; else exotic/flat.
pub const USDC_MINT: Pubkey =
    Pubkey::from_str_const("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
/// Curve offsets (after the 8-byte Anchor disc):
/// complete @48, creator @49..81, mayhem @81, cashback @82,
/// quote @83..115 (zero = SOL-paired -> WSOL).
pub const CURVE_CREATOR_OFFSET: usize = 49;
/// Creator slot end (exclusive).
pub const CURVE_CREATOR_END: usize = 81;
/// Mayhem flag byte.
pub const CURVE_MAYHEM_FLAG_OFFSET: usize = 81;
/// Cashback flag byte.
pub const CURVE_CASHBACK_FLAG_OFFSET: usize = 82;
/// Quote-mint slot start.
pub const CURVE_QUOTE_MINT_OFFSET: usize = 83;
/// Quote-mint slot end (exclusive).
pub const CURVE_QUOTE_MINT_END: usize = 115;
/// Complete flag (1 = migrated; curve trades refused).
pub const CURVE_COMPLETE_OFFSET: usize = 48;
/// Coin creator rate (u64le bps; 0 = unconfigured). Needs `115..123`.
pub const CURVE_COIN_CREATOR_FEE_OFFSET: usize = 115;
/// Creator-fee slot end (exclusive).
pub const CURVE_COIN_CREATOR_FEE_END: usize = 123;
/// `creator_fee_configurable` byte in Global (1088 bytes).
pub const GLOBAL_FEE_GATE_OFFSET: usize = 1045;
/// Max decoded fee tiers per FeeConfig table (DoS bound; live uses 1+1).
/// Single source for the executor quote decoder and the paper fee reader.
pub const MAX_FEE_TIERS: usize = 1024;
/// Seed for user volume PDAs: `["user_volume_accumulator", user]`.
pub const USER_VOLUME_ACCUMULATOR_SEED: &[u8] = b"user_volume_accumulator";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_match_canonical_strings() {
        assert_eq!(
            PUMP_PROGRAM_ID.to_string(),
            "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"
        );
        assert_eq!(PUMP_PROGRAM_ID.to_string(), PUMP_PROGRAM_ID_STR);
        assert_eq!(
            PUMP_MIGRATION_AUTHORITY.to_string(),
            "39azUYFWPz3VHgKCf3VChUwbpURdCHRxjWVowf5jUJjg"
        );
    }

    #[test]
    fn event_discriminators_follow_anchor_convention() {
        // Anchor event discs are `sha256("event:<Name>")[..8]` — the
        // same bytes the chain emits in logs and `emit_cpi` ixs. This
        // pins the constants to the convention, not to each other;
        // OBSERVATION (not this test) is what admits an event:
        // Trade/Complete matched live logs, PostComplete has no
        // capture yet (see the const docs).
        use sha2::{Digest, Sha256};
        let disc = |name: &str| -> [u8; 8] {
            let hash = Sha256::digest(format!("event:{name}").as_bytes());
            hash[..8].try_into().expect("8 bytes")
        };
        assert_eq!(disc("TradeEvent"), TRADE_EVENT_DISCRIMINATOR);
        assert_eq!(disc("CompleteEvent"), COMPLETE_EVENT_DISCRIMINATOR);
        assert_eq!(
            disc("PostCompleteBuyEvent"),
            POST_COMPLETE_BUY_EVENT_DISCRIMINATOR
        );
    }

    #[test]
    fn discriminators_are_pairwise_distinct() {
        // A copy would alias two kinds in one parser arm.
        let discs = [
            PUMP_CREATE_DISCRIMINATOR,
            PUMP_CREATE_V2_DISCRIMINATOR,
            PUMP_BUY_DISCRIMINATOR,
            PUMP_SELL_DISCRIMINATOR,
            PUMP_BUY_V2_DISCRIMINATOR,
            PUMP_SELL_V2_DISCRIMINATOR,
            PUMP_BUY_V3_DISCRIMINATOR,
            PUMP_SELL_V3_DISCRIMINATOR,
            PUMP_BUY_EXACT_QUOTE_IN_V3_DISCRIMINATOR,
            PUMP_MIGRATE_DISCRIMINATOR,
            PUMP_MIGRATE_V2_DISCRIMINATOR,
            TRADE_EVENT_DISCRIMINATOR,
            COMPLETE_EVENT_DISCRIMINATOR,
            POST_COMPLETE_BUY_EVENT_DISCRIMINATOR,
        ];
        for (i, a) in discs.iter().enumerate() {
            for b in &discs[i + 1..] {
                assert_ne!(a, b, "aliased discriminator");
            }
        }
    }
}
