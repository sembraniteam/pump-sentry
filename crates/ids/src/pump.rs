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
/// Verified against a real mainnet `buy_v2` (27-account instruction in
/// the frozen `create_v2` launch tx): payload is 16 bytes, consistent
/// with `(amount: u64, max_sol_cost: u64)` like legacy `buy`.
/// NOTE: single-sample inference for the args layout; the discriminator
/// and trader position below are certain, the amount semantic is not.
/// `sell_v2` is still unknown (discriminator and layout unverified) and
/// deliberately unhandled — dev dumps routed through it are invisible to
/// the gate (documented residual blindness, fail-open on that path).
pub const PUMP_BUY_V2_DISCRIMINATOR: [u8; 8] = [184, 23, 238, 97, 103, 197, 211, 61];
/// `sell_v2` instruction discriminator (docs-sourced, NOT mainnet-
/// observed: no verified sample yet, so no classifier is built on it).
/// Present so future detection work has one canonical value to confirm.
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
/// Seed for creator-vault PDAs: `["creator-vault", creator]` under the
/// Pump program. Verified byte-exact against a live mayhem buy.
pub const CREATOR_VAULT_SEED: &[u8] = b"creator-vault";
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
}
