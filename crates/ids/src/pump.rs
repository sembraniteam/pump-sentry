//! Pump.fun program addresses and instruction discriminators.

use solana_pubkey::Pubkey;

/// Pump.fun program.
pub const PUMP_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
/// Pump.fun program ID in string form, for RPC filters
/// (`RpcTransactionLogsFilter::Mentions`) that take strings, not `Pubkey`.
/// Kept next to [`PUMP_PROGRAM_ID`] so a program upgrade edits one file;
/// the equality test below makes drift a build break, not silent mainnet
/// misbehavior.
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

/// `create` instruction discriminator (skill solana-geyser-pumpfun).
pub const PUMP_CREATE_DISCRIMINATOR: [u8; 8] = [24, 30, 200, 40, 5, 28, 7, 119];
/// `create_v2` instruction discriminator (launches with creator/mayhem
/// fields; verified against the canonical Pump.fun IDL snapshot).
/// Parsed by `parse_pump_create_v2` with the 14-account IDL layout.
pub const PUMP_CREATE_V2_DISCRIMINATOR: [u8; 8] = [214, 144, 76, 236, 95, 139, 49, 180];
/// `buy` instruction discriminator (skill solana-pumpfun-swap).
pub const PUMP_BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
/// `sell` instruction discriminator (skill solana-pumpfun-swap).
pub const PUMP_SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
/// `buy_v2` instruction discriminator: v2/mayhem bonding-curve buys.
/// Chain-proven 2026-10-11 against TWO frozen mainnet `buy_v2` txs
/// (27-account instructions): accounts[13] is the fee payer and payload
/// bytes 8..16 equal the buyer's token-balance delta to the unit in both
/// (3_485_709_524_202 and 8_671_591_758_466). Payload is 16 bytes:
/// `(amount: u64, max_sol_cost: u64)` like legacy `buy` (the second
/// sample's guard is `u64::MAX` — an open ape with no slippage cap).
pub const PUMP_BUY_V2_DISCRIMINATOR: [u8; 8] = [184, 23, 238, 97, 103, 197, 211, 61];
/// `buy_v3` instruction discriminator: 17-account bonding-curve buys
/// (official `pump-public-docs`, TRADE_V3.md). Same price/fees as
/// `buy_v2`; the protocol+creator fees stay on the curve instead of
/// paying out. Verified by construction: `sha256("global:buy_v3")[..8]`
/// reproduces these bytes, and the same method reproduces all seven
/// frozen-sample discriminators below exactly (see
/// `discriminators_are_pairwise_distinct` neighbors + offline check).
pub const PUMP_BUY_V3_DISCRIMINATOR: [u8; 8] = [7, 5, 29, 196, 245, 23, 101, 80];
/// `sell_v3` instruction discriminator: 17-account bonding-curve sells.
/// Same source and verification as [`PUMP_BUY_V3_DISCRIMINATOR`].
pub const PUMP_SELL_V3_DISCRIMINATOR: [u8; 8] = [28, 146, 222, 119, 38, 196, 105, 213];
/// `buy_exact_quote_in_v3` instruction discriminator: exact-spend
/// 17-account buys (`spendable_quote_in`, `min_tokens_out`). Same source
/// and verification as [`PUMP_BUY_V3_DISCRIMINATOR`].
pub const PUMP_BUY_EXACT_QUOTE_IN_V3_DISCRIMINATOR: [u8; 8] =
    [225, 247, 80, 30, 213, 179, 132, 136];
/// `sell_v2` instruction discriminator: v2/mayhem bonding-curve sells.
/// Mainnet-verified 2026-10-11 against a frozen `sell_v2` (26-account
/// instruction, `sell_v2_mainnet.json`): accounts[13] is the fee payer
/// (same trader position as `buy_v2`), and payload bytes 8..16 are the
/// sold base units to the unit (fee payer's token balance decreased by
/// exactly that amount). Payload is 24 bytes: discriminator + `(amount:
/// u64, min_sol_output: u64)` mirroring legacy `sell` (two-sample
/// confirmed 2026-10-13). The gate classifies these as `Sell` (dev dumps
/// abort); live dispatch is v3-only, so cashback-era `sell_v2` curves
/// fail closed to manual exit (no v2 builder remains).
pub const PUMP_SELL_V2_DISCRIMINATOR: [u8; 8] = [93, 246, 130, 60, 231, 233, 64, 178];
/// `migrate` instruction discriminator (skill solana-raydium-migration).
pub const PUMP_MIGRATE_DISCRIMINATOR: [u8; 8] = [155, 234, 231, 146, 236, 158, 162, 30];
/// Global volume accumulator: every swap's volume tracking roots here.
/// Byte-verified against live mayhem buys (position 12).
pub const GLOBAL_VOLUME_ACCUMULATOR: Pubkey =
    Pubkey::from_str_const("Hq2wp8uJ9jCPsYgNHex8RtqdvMPfVGoYwjvF1ATiwn2Y");
/// Fee configuration account: swap fee schedule (position 14).
pub const FEE_CONFIG_ACCOUNT: Pubkey =
    Pubkey::from_str_const("8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt");
/// Fee configuration program: resolves fee routing (position 15).
pub const FEE_CONFIG_PROGRAM: Pubkey =
    Pubkey::from_str_const("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
/// Buyback fee recipients: the 8 published wallets (`pump-public-docs`
/// FEE_RECIPIENTS.md). Any one serves as `buyback_fee_recipient` on
/// every coin, v2 and v3 alike (v3 needs NOTHING else fee-side: the
/// protocol+creator fees stay on the curve). Default pick is index 0 —
/// deterministic and testable; the docs' random-per-tx spreading is an
/// operational option, not a correctness requirement.
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
/// Default buyback pick (index 0): deterministic across builds, so
/// frozen-sample tests pin exact bytes.
pub const DEFAULT_BUYBACK_FEE_RECIPIENT: Pubkey = BUYBACK_FEE_RECIPIENTS[0];
/// USDC mint: quotes in this mint route to the `stable_fee_tiers`
/// schedule (official fee math). SOL-like quotes (zero key, either WSOL
/// program id) route to `fee_tiers`; anything else to exotic/flat.
pub const USDC_MINT: Pubkey =
    Pubkey::from_str_const("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
/// Seed for creator-vault PDAs: `["creator-vault", creator]` under the
/// Pump program. Verified byte-exact against a live mayhem buy.
pub const CREATOR_VAULT_SEED: &[u8] = b"creator-vault";
/// Seed for sharing-config PDAs: `["sharing-config", base_mint]` under
/// the fee program. Verified byte-exact (bump 255): both frozen `buy_v2`
/// samples AND both frozen `sell_v2` samples carry it at position [18].
pub const SHARING_CONFIG_SEED: &[u8] = b"sharing-config";
/// Bonding-curve account field offsets (after the 8-byte Anchor
/// discriminator), verified live 2026-10-14 against two mainnet curves
/// (a legacy-18-buy curve and a `buy_v2` curve, both 166 bytes):
/// creator @49..81 (vault-PDA cross-checked on both), mayhem flag @81
/// (1 on the reserved-fee curve, 0 on the normal-fee curve — matches the
/// fee-list the curve's fills use), cashback flag @82 (0 on both),
/// quote mint @83..115 (zero key on both = SOL-paired → WSOL rule).
pub const CURVE_CREATOR_OFFSET: usize = 49;
/// End offset (exclusive) of the creator slot in curve data.
pub const CURVE_CREATOR_END: usize = 81;
/// Offset of the mayhem-mode flag byte in curve data.
pub const CURVE_MAYHEM_FLAG_OFFSET: usize = 81;
/// Offset of the cashback-coin flag byte in curve data.
pub const CURVE_CASHBACK_FLAG_OFFSET: usize = 82;
/// Offset of the quote-mint slot in curve data.
pub const CURVE_QUOTE_MINT_OFFSET: usize = 83;
/// End offset (exclusive) of the quote-mint slot in curve data.
pub const CURVE_QUOTE_MINT_END: usize = 115;
/// Minimum curve-data length for the length gate (all eras —
/// pre-extension 115-byte curves included). Note: the coin-creator fee
/// below needs `115..123`, so 115..122-byte blobs pass this gate and
/// then read `None` (fail-closed to the reference path) — intended.
pub const CURVE_MIN_LEN: usize = 115;
/// Offset of the complete flag byte in curve data (verified live:
/// 1 on two migrated curves with zeroed reserves).
pub const CURVE_COMPLETE_OFFSET: usize = 48;
/// Offset of the coin's own creator-fee rate (u64le bps, 0 = unconfigured
/// → schedule rate) in curve data. Proven by the executor's dispatch
/// path (`CurveState::coin_creator_fee_bps` reads here); needs
/// `115..123`, so shorter blobs fail closed to the default tier.
pub const CURVE_COIN_CREATOR_FEE_OFFSET: usize = 115;
/// End offset (exclusive) of the coin-creator-fee slot in curve data.
pub const CURVE_COIN_CREATOR_FEE_END: usize = 123;
/// Offset of the `creator_fee_configurable` gate byte in the Global
/// account (1088 bytes; 1 = the coin's own rate wins when nonzero).
/// Proven transitively by the buyback array at 741..997 matching the
/// published 8-list in order (see the executor's fee engine).
pub const GLOBAL_FEE_GATE_OFFSET: usize = 1045;
/// Seed for user volume-accumulator PDAs: `["user_volume_accumulator",
/// user]`. Verified byte-exact against a live mayhem buy.
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
    fn discriminators_are_pairwise_distinct() {
        // A copy-paste discriminator would alias two instruction kinds
        // into one parser arm (buys read as sells or vice versa).
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
        ];
        for (i, a) in discs.iter().enumerate() {
            for b in &discs[i + 1..] {
                assert_ne!(a, b, "aliased discriminator");
            }
        }
    }
}
