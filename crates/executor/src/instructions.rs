//! Pump.fun buy/sell instruction builders (skill `solana-pumpfun-swap`).
//!
//! Layouts and account order follow the skill exactly. Program addresses
//! are compile-time constants; SPL program IDs cross-check against the
//! `spl-token` / `spl-associated-token-account` crates so version drift
//! fails loudly in tests instead of on-chain.
//!
//! SHAPES BUILT HERE:
//! - 12-account legacy `buy`/`sell` core (classic curves).
//! - 16-account extended mayhem buy prefix (RETIRED as a dispatch
//!   shape — buyback-enforcing curves reject it with 6062, proven live
//!   2026-10-13 off a fresh `create_v2` launch): positions 0..16 stay
//!   byte-exact against a frozen mainnet mayhem buy, and the
//!   [`MayhemRouting`] tail ([16]/[17]) grows the prefix to the chain's
//!   full 18 accounts. Dispatch never sends the bare prefix —
//!   [`SwapExecutor::preflight_buy`] fails closed without a same-curve
//!   reference, and creator curves otherwise take the v3 fast path or
//!   the 27-account `buy_v2` path. The prefix builder arm exists so the
//!   18-account construction and its byte-proof share one body.
//! - 27-account v2 `buy_v2` shape ([`PumpSwapBuilder::build_buy_v2_ix`]):
//!   the exact v2-buy layout — the `sell_v2` layout plus the global
//!   volume accumulator at [19] — verified byte-exact (positions +
//!   payload, two frozen samples). Required for entries on v2 curves,
//!   where the 18-account mayhem buy fails (frozen failed sample).
//! - 26-account v2 `sell_v2` shape ([`PumpSwapBuilder::build_sell_v2_ix`]):
//!   the exact mayhem-sell layout, verified byte-exact (positions AND
//!   mutability) against the frozen `sell_v2_mainnet` sample. Required
//!   for exits on v2 curves — legacy sells are rejected there.
//! - 17-account v3 `buy_v3`/`sell_v3` shapes
//!   ([`PumpSwapBuilder::build_buy_v3_ix`]/[`PumpSwapBuilder::build_sell_v3_ix`]):
//!   the official TRADE_V3.md layouts — fees stay on the curve, so no
//!   fee recipient, creator vault, or sharing config travels. `sell_v3`
//!   is byte-exact vs a frozen mainnet sample (positions + payload +
//!   re-derived PDA/ATAs); `buy_v3` awaits its frozen sample (same 17
//!   positions by construction — one shared body builds both sides).
//!
//! ASSUMPTIONS REQUIRING MAINNET PROOF BEFORE FUNDING:
//! - Mayhem buyback-routing accounts [16]/[17] have no static derivation:
//!   resolve per trade from a same-curve reference buy (the ignored
//!   layout-acceptance test is the gate).
//! - `buy_v2` opaque routing accounts [6] (fee recipient), [2] (quote
//!   mint), [8]/[9] (pfee vault + quote account) have no proven
//!   derivation: resolve per trade from a same-curve reference buy_v2,
//!   never guess. Position [18] is the derived sharing-config PDA
//!   (bump 255, proven on all four frozen v2 samples). Writability
//!   flags mirror the proven `sell_v2` set plus writable global-vol at
//!   [19] — the 27-shape sim probe is the gate that proves the flags.
//! - `sell_v2` opaque routing accounts [8]/[9] have no proven
//!   derivation (pfee vault, its quote account, one readonly): resolve
//!   per trade from curve state or a same-curve reference sell_v2.
//!   Quote-token fills may additionally route a filler leg (seen once);
//!   SOL-quoted fills settle curve-direct (seen once) — the path this
//!   pipeline trades.
//!
//! RUNTIME (not build-time) ROUTING — never hardcoded here:
//! - Fee recipient resolves live (override → Global account → canonical
//!   constant) on classic paths; the program rotates recipients.
//!   Mayhem paths (v2 buys, `sell_v2`) take the fee from the same-curve
//!   reference instead: fee sets rotate globally while curves keep
//!   working, and a mixed-era set fails authorization — only the
//!   reference set simulates clean (proven both ways on mainnet).
//! - Token program resolves per mint (classic SPL vs Token-2022); the
//!   ATA derives bound to it (a 2022 mint through the classic program
//!   fails with `IncorrectProgramId`, verified live).

use borsh::{BorshDeserialize, BorshSerialize};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use std::str::FromStr;

pub use ids::{
    ASSOCIATED_TOKEN_PROGRAM_ID, BUYBACK_FEE_RECIPIENTS, CREATOR_VAULT_SEED,
    CURVE_CASHBACK_FLAG_OFFSET, CURVE_COMPLETE_OFFSET, CURVE_CREATOR_END, CURVE_CREATOR_OFFSET,
    CURVE_MAYHEM_FLAG_OFFSET, CURVE_MIN_LEN, CURVE_QUOTE_MINT_END, CURVE_QUOTE_MINT_OFFSET,
    DEFAULT_BUYBACK_FEE_RECIPIENT, EVENT_AUTHORITY, FEE_CONFIG_ACCOUNT, FEE_CONFIG_PROGRAM,
    FEE_RECIPIENT, GLOBAL_ACCOUNT, GLOBAL_VOLUME_ACCUMULATOR, PUMP_BUY_DISCRIMINATOR,
    PUMP_BUY_V2_DISCRIMINATOR, PUMP_BUY_V3_DISCRIMINATOR, PUMP_BUY_EXACT_QUOTE_IN_V3_DISCRIMINATOR,
    PUMP_CREATE_V2_DISCRIMINATOR, PUMP_PROGRAM_ID, PUMP_SELL_DISCRIMINATOR, PUMP_SELL_V2_DISCRIMINATOR,
    PUMP_SELL_V3_DISCRIMINATOR, RENT_SYSVAR_ID, SHARING_CONFIG_SEED, SYSTEM_PROGRAM_ID,
    TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, USDC_MINT, USER_VOLUME_ACCUMULATOR_SEED,
    WSOL_MINT,
};

/// Buy args: token units requested plus max SOL spend (slippage guard
/// and volume tracking). The deployed program reads a trailing plain
/// bool (live 25-byte payloads end in 0x00/0x01; the SDK snapshot's
/// Option<bool> over-reads by one byte and mismatches chain bytes).
#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq)]
pub struct PumpBuyArgs {
    /// Token units requested (base units).
    pub amount: u64,
    /// Maximum SOL in lamports allowed (slippage guard).
    pub max_sol_cost: u64,
    /// Volume-accumulator tracking (mirrors live flow; default `true`).
    pub track_volume: bool,
}

/// Sell args: token units to sell + min SOL out (slippage guard).
#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq)]
pub struct PumpSellArgs {
    /// Token units to sell (base units).
    pub amount: u64,
    /// Minimum SOL in lamports to receive (slippage guard).
    pub min_sol_output: u64,
}

/// v2 sell args: token units to sell plus the SOL floor.
/// Payload is 24 bytes (discriminator + two u64s), like legacy `sell`.
/// The second u64 is the seller's minimum SOL proceeds (lamports):
/// two-sample confirmed (107,092,467 ≥ 30,205 and 94,304,473 ≥
/// 65,131,943 in frozen `sell_v2` fills). A filler quote-token leg may
/// additionally appear on quote-token fills — filler arrangement, not
/// part of this payload.
#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq)]
pub struct PumpSellV2Args {
    /// Token units to sell (base units) — chain-proven exact.
    pub amount: u64,
    /// Minimum SOL in lamports to receive (slippage guard).
    pub min_sol_output: u64,
}

/// v2 buy args: token units requested plus max SOL spend (slippage
/// guard). Payload is 24 bytes (discriminator + two u64s): NO trailing
/// `track_volume` bool, unlike legacy `buy` (both frozen `buy_v2`
/// samples end after the second u64). The first u64 is chain-proven
/// exact (buyer token-balance delta to the unit, two independent
/// samples); the second u64 is `u64::MAX` in both samples (open ape,
/// no slippage cap) — the caller always sets its own guard, never a
/// copied value.
#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq)]
pub struct PumpBuyV2Args {
    /// Token base units requested.
    pub amount: u64,
    /// Maximum SOL in lamports allowed (slippage guard).
    pub max_sol_cost: u64,
}

/// v3 buy args: token units requested plus max quote spend (slippage
/// guard, fees included per TRADE_V3.md). Payload is 24 bytes when the
/// trailing `partial_fill` flag is omitted (the safe default on mayhem
/// coins: over-left buys fail with `NotEnoughTokensToBuy` instead of
/// running past the curve end).
#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq)]
pub struct PumpBuyV3Args {
    /// Token base units requested.
    pub amount: u64,
    /// Maximum quote in base units allowed (slippage guard, fees in).
    pub max_sol_cost: u64,
}

/// v3 sell args: token units to sell plus the quote floor. Payload is
/// 24 bytes (discriminator + two u64s), like `sell_v2`.
#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq)]
pub struct PumpSellV3Args {
    /// Token base units to sell.
    pub amount: u64,
    /// Minimum quote in base units to receive (slippage guard).
    pub min_sol_output: u64,
}

/// v3 exact-quote buy args: spend exactly `spendable_quote_in` for at
/// least `min_tokens_out` tokens. Payload is 24 bytes (no
/// `partial_fill` tail). Same 17 accounts as `buy_v3` — enable in
/// dispatch only after its frozen-sample byte test + sim probe turn
/// green (the saved `buy_exact_quote_in_v3_mainnet.json` fixture is
/// that future test's input).
#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq)]
pub struct PumpBuyExactQuoteInV3Args {
    /// Exact quote to spend, fees included.
    pub spendable_quote_in: u64,
    /// Fewest base tokens accepted.
    pub min_tokens_out: u64,
}

/// Stateless builder for Pump.fun swap instructions.
///
/// LIVE SHAPE STATUS (verified 2026-10-11 against mainnet, extended
/// 2026-10-13/14; see the ignored layout-acceptance tests, the gates
/// that must turn green):
/// the program's current mayhem buy is 18 accounts. Positions 0–15 are
/// FULLY MAPPED and byte-proven against frozen samples (legacy core +
/// live token program + Global recipient + creator vault + volume/fee
/// routing). Positions [16],[17] are per-curve buyback-routing accounts
/// with no static derivation: [17] is a pfee-owned vault from a small
/// rotating set shared across curves AND across the 27-account buy_v2
/// shape at [8] (three landed 18-account buys, Oct-2026 block scan:
/// three curves, three distinct [16]s, vaults EHAAi/5eHhj/5YxQF —
/// per-curve copying, never hardcoding); [16] is unmapped (per-curve
/// unique). Omitting them fails simulation with
/// `BuybackFeeRecipientMissing` (6062) on enforcing curves. ~100 PDA
/// seed combinations across pump/mayhem/pfee programs found nothing.
/// Resolution, not derivation: copy [16],[17] from a same-curve
/// reference buy at trade time (`MayhemRouting` +
/// `select_mayhem_routing`), preferably the watchlist gate's first
/// successful buy; without a reference the dispatch fails closed
/// (every static shape is rejected on creator curves).
///
/// V2 curves take `buy_v2` (27 accounts, own discriminator) instead of
/// the 18-account buy — the 18-account shape fails there (frozen failed
/// sample, 6002). The builder covers it
/// ([`PumpSwapBuilder::build_buy_v2_ix`], byte-exact vs two frozen
/// samples); opaque routing resolves per trade from a same-curve
/// reference buy_v2, fail-closed when none scans.
///
/// NOTE — live shape drift (verified 2026-10-11 against mainnet): the
/// program's current mayhem flow extends this 12-account core to 18
/// (buy) / 16 (sell) accounts with the SAME discriminators. Positions
/// verified byte-exact against live samples: legacy 0–8 (with the
/// mint's OWN token program at 8: classic SPL or Token-2022),
/// event-auth(10), pump(11), global-vol-accum(12, constant),
/// user-vol-accum PDA(13, derived), fee-config(14, constant),
/// fee-config-program(15, constant). Still unmapped: buy [9],[16],[17]
/// and sell [8],[14],[15] (per-launch PDAs; creator-vault needs the
/// creator). Until those resolve, `simulateTransaction` rejects this
/// core on mayhem curves — see the ignored layout-acceptance test, which
/// is the gate that must turn green before funding.
///
/// NOTE — v2 curves take a different path entirely (verified 2026-10-13):
/// `sell_v2` is its own 26-account instruction (own discriminator), NOT
/// an extension of the shapes above. Live v2 exits build it via
/// [`PumpSwapBuilder::build_sell_v2_ix`]; the experimental 16-account
/// extended sell is retained for API compatibility but no live path
/// selects it anymore.
pub struct PumpSwapBuilder;

/// Account set for one bonding-curve swap, bundled so new routing
/// accounts cannot grow the builder arity again (mayhem flow already
/// carries volume/fee-config accounts past the 12-account core).
#[derive(Debug, Clone, Copy)]
pub struct SwapAccounts<'a> {
    /// Fee payer and signer (also the trader).
    pub payer: &'a Pubkey,
    /// Token mint.
    pub mint: &'a Pubkey,
    /// Bonding-curve PDA.
    pub bonding_curve: &'a Pubkey,
    /// Curve's associated token account.
    pub associated_bonding_curve: &'a Pubkey,
    /// Trader's associated token account.
    pub user_ata: &'a Pubkey,
    /// Fee recipient: live resolution (override, Global, then constant)
    /// on classic paths; the same-curve reference on mayhem paths
    /// (live Global fails authorization there — see [`MayhemRouting`]).
    pub fee_recipient: &'a Pubkey,
    /// Token program owning `mint` (classic SPL vs Token-2022).
    pub token_program: &'a Pubkey,
    /// Token creator: enables the 16-account extended shape (creator
    /// vault at 9 plus volume/fee-config routing at 12–15), verified
    /// against live mayhem buys. `None` keeps the legacy 12-account
    /// core (classic curves; rejected on mayhem curves).
    pub creator: Option<&'a Pubkey>,
    /// Mayhem buyback tail ([16]/[17]) for buys on mayhem curves:
    /// appended after the 16-account shape to reach the chain's full
    /// 18 accounts. Resolved per trade from a same-curve reference buy
    /// (see [`MayhemRouting`]); `None` keeps the 16-account shape and
    /// lets preflight simulation decide (6062 where enforced).
    pub mayhem_routing: Option<MayhemRouting<'a>>,
}

/// Mayhem buyback-routing tail for the 18-account buy shape: position
/// [16] (unmapped pre-first-buy ephemeral) and [17] (pfee-owned
/// buyback vault — the same vault appears as buy_v2 position [8],
/// verified identical across curves). No static derivation exists for
/// either: resolve per trade from a same-curve reference buy
/// ([`select_mayhem_routing`]), never guess. Mutability follows vault
/// convention (writable); preflight simulation gates every build.
#[derive(Debug, Clone, Copy)]
pub struct MayhemRouting<'a> {
    /// Position 16 account from the reference buy.
    pub unknown_16: &'a Pubkey,
    /// Position 17 pfee buyback vault from the reference buy.
    pub pfee_vault: &'a Pubkey,
}

/// Account set for a v2 (`sell_v2`) mayhem-curve sell: the full
/// 26-account shape, verified byte-exact (positions AND mutability,
/// from `jsonParsed` account flags) against the frozen `sell_v2_mainnet`
/// sample. Required for exits on v2 curves — legacy sells are rejected
/// there (unclosable positions otherwise).
///
/// Provenance per position (verified 2026-10-13):
/// - caller-supplied: payer, base/quote mints, curve, assoc curve, live
///   fee recipient, per-mint token programs, creator;
/// - builder-derived: all five quote-side ATAs (fee-recipient, curve,
///   creator-vault, user-vol-accum, and user quote ATAs via ATA seeds),
///   creator vault + user-vol-accum PDAs (verified: derived addresses
///   match the fixture's [16]/[19] exactly);
/// - reference-sourced (no proven derivation — resolve per trade from
///   a same-curve reference sell_v2): quote mint [2] (also readable at
///   curve-account offset 83, zero → WSOL), pfee vault [8], its quote
///   account [9];
/// - builder-derived: sharing-config PDA [18] (verified bump 255 on both
///   frozen samples).
#[derive(Debug, Clone, Copy)]
pub struct SellV2Accounts<'a> {
    /// Fee payer and signer (also the trader, position 13).
    pub payer: &'a Pubkey,
    /// Sold token mint (position 1).
    pub mint: &'a Pubkey,
    /// Quote mint for fee routing (position 2).
    pub quote_mint: &'a Pubkey,
    /// Bonding-curve PDA (position 10).
    pub bonding_curve: &'a Pubkey,
    /// Curve's base-inventory ATA (position 11).
    pub associated_bonding_curve: &'a Pubkey,
    /// Fee recipient (position 6). Resolve from the same-curve
    /// reference, NOT live Global (see [`MayhemReference`]
    /// era-consistency).
    pub fee_recipient: &'a Pubkey,
    /// Token program owning `mint` (classic SPL vs Token-2022).
    pub base_token_program: &'a Pubkey,
    /// Token program owning `quote_mint`.
    pub quote_token_program: &'a Pubkey,
    /// Token creator (creator-vault PDA derives from it).
    pub creator: &'a Pubkey,
    /// pfee quote vault, position 8 (reference-sourced).
    pub pfee_vault: &'a Pubkey,
    /// pfee vault's quote account, position 9 (reference-sourced).
    pub pfee_quote_account: &'a Pubkey,
}

/// Account set for a v2 (`buy_v2`) mayhem-curve buy: the full 27-account
/// shape — the `sell_v2` layout plus the global volume accumulator at
/// [19] (buys track volume globally; sells don't). Verified byte-exact
/// (positions + payload, both frozen `buy_v2` samples; creator vault +
/// user-vol PDA + all six base/quote ATAs re-derived, not copied).
/// Required for entries on v2 curves — the 18-account mayhem buy fails
/// there (frozen failed sample), and legacy 12-account buys fail with
/// `BuybackFeeRecipientMissing` (6062).
///
/// Provenance per position (verified 2026-10-14, two samples):
/// - caller-supplied: payer, base/quote mints, curve, assoc curve, fee
///   recipient, per-mint token programs, creator;
/// - builder-derived: creator vault [16] + user-vol accumulator [20]
///   PDAs (verified: derived addresses match both fixtures exactly),
///   all six base/quote ATAs ([7] fee, [12] curve, [14] user base,
///   [15] user quote, [17] creator-vault, [21] user-vol — verified at
///   exact bumps, which vary per address);
/// - reference-sourced (no proven derivation — resolve per trade from
///   a same-curve reference buy_v2): quote mint [2], fee recipient [6],
///   pfee vault [8], its quote account [9];
/// - builder-derived: sharing-config PDA [18] (verified bump 255 on both
///   frozen samples).
#[derive(Debug, Clone, Copy)]
pub struct BuyV2Accounts<'a> {
    /// Fee payer and signer (also the trader, position 13).
    pub payer: &'a Pubkey,
    /// Bought token mint (position 1).
    pub mint: &'a Pubkey,
    /// Quote mint for fee routing (position 2; WSOL in both samples).
    pub quote_mint: &'a Pubkey,
    /// Bonding-curve PDA (position 10).
    pub bonding_curve: &'a Pubkey,
    /// Curve's base-inventory ATA (position 11).
    pub associated_bonding_curve: &'a Pubkey,
    /// Fee recipient (position 6). Resolve from the same-curve
    /// reference, NOT live Global (era-consistency, like mayhem buys
    /// and `sell_v2`: a mixed-era set fails authorization).
    pub fee_recipient: &'a Pubkey,
    /// Token program owning `mint` (classic SPL vs Token-2022; v2 mints
    /// live under Token-2022).
    pub base_token_program: &'a Pubkey,
    /// Token program owning `quote_mint`.
    pub quote_token_program: &'a Pubkey,
    /// Token creator (creator-vault PDA derives from it; in both frozen
    /// samples the creator is the launcher — create_v2 position 5).
    pub creator: &'a Pubkey,
    /// pfee quote vault, position 8 (reference-sourced).
    pub pfee_vault: &'a Pubkey,
    /// pfee vault's quote account, position 9 (reference-sourced).
    pub pfee_quote_account: &'a Pubkey,
}

/// Account set for a v3 (`buy_v3`) buy: the 17-account shape from the
/// official TRADE_V3.md table. No fee recipient (fees stay on the
/// curve), no creator vault, no sharing config — the only external
/// fee-side account is the buyback recipient (any of the 8 published
/// [`BUYBACK_FEE_RECIPIENTS`]; must exist — pre-create its quote ATA
/// idempotently for token-paired quotes).
///
/// Provenance: caller-supplied (payer, mints, curve, assoc curve,
/// per-mint programs, buyback pick); builder-derived (user-vol PDA
/// [11], base/quote user ATAs [9]/[10], curve base/quote ATAs [6]/[7]).
#[derive(Debug, Clone, Copy)]
pub struct BuyV3Accounts<'a> {
    /// Fee payer and signer (also the trader, position 8).
    pub payer: &'a Pubkey,
    /// Bought token mint (position 1).
    pub mint: &'a Pubkey,
    /// Quote mint (position 2; WSOL for SOL-paired coins).
    pub quote_mint: &'a Pubkey,
    /// Bonding-curve PDA (position 5).
    pub bonding_curve: &'a Pubkey,
    /// Curve's base-inventory ATA (position 6).
    pub associated_bonding_curve: &'a Pubkey,
    /// Token program owning `mint`.
    pub base_token_program: &'a Pubkey,
    /// Token program owning `quote_mint`.
    pub quote_token_program: &'a Pubkey,
    /// Buyback recipient (position 13): one of [`BUYBACK_FEE_RECIPIENTS`]
    /// for SOL-paired coins, its quote ATA for token-paired quotes.
    pub buyback_recipient: &'a Pubkey,
}

/// Account set for a v3 (`sell_v3`) sell: same 17 accounts as
/// [`BuyV3Accounts`], drain direction.
#[derive(Debug, Clone, Copy)]
pub struct SellV3Accounts<'a> {
    /// Fee payer and signer (also the trader, position 8).
    pub payer: &'a Pubkey,
    /// Sold token mint (position 1).
    pub mint: &'a Pubkey,
    /// Quote mint (position 2; WSOL for SOL-paired coins).
    pub quote_mint: &'a Pubkey,
    /// Bonding-curve PDA (position 5).
    pub bonding_curve: &'a Pubkey,
    /// Curve's base-inventory ATA (position 6).
    pub associated_bonding_curve: &'a Pubkey,
    /// Token program owning `mint`.
    pub base_token_program: &'a Pubkey,
    /// Token program owning `quote_mint`.
    pub quote_token_program: &'a Pubkey,
    /// Buyback recipient (position 13): one of [`BUYBACK_FEE_RECIPIENTS`]
    /// for SOL-paired coins, its quote ATA for token-paired quotes.
    pub buyback_recipient: &'a Pubkey,
}

/// Creator-vault PDA for `creator`: `["creator-vault", creator]`.
/// Seed and derivation verified byte-exact against a live mayhem buy.
pub fn creator_vault_pda(creator: &Pubkey) -> Pubkey {
    let (pda, _) = Pubkey::find_program_address(
        &[CREATOR_VAULT_SEED, creator.as_ref()],
        &PUMP_PROGRAM_ID,
    );
    pda
}

/// User volume-accumulator PDA: `["user_volume_accumulator", user]`.
/// Seed and derivation verified byte-exact against a live mayhem buy.
pub fn user_volume_accumulator_pda(user: &Pubkey) -> Pubkey {
    let (pda, _) = Pubkey::find_program_address(
        &[USER_VOLUME_ACCUMULATOR_SEED, user.as_ref()],
        &PUMP_PROGRAM_ID,
    );
    pda
}

/// Sharing-config PDA for `mint`: `["sharing-config", mint]` under the
/// fee program (official BUY.md row 19). Verified byte-exact (bump 255)
/// against both frozen `buy_v2` samples AND both frozen `sell_v2`
/// samples at position [18] — derived, never reference-copied.
pub fn sharing_config_pda(mint: &Pubkey) -> Pubkey {
    let (pda, _) = Pubkey::find_program_address(
        &[SHARING_CONFIG_SEED, mint.as_ref()],
        &FEE_CONFIG_PROGRAM,
    );
    pda
}

/// Swap direction with its slippage-guarded limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapKind {
    /// Buy `amount` base units for at most `max_sol_cost` lamports.
    Buy {
        /// Token base units requested.
        amount: u64,
        /// Maximum SOL in lamports allowed.
        max_sol_cost: u64,
        /// Volume-accumulator tracking (plain bool per live wire bytes).
        track_volume: bool,
    },
    /// Sell `amount` base units for at least `min_sol_output` lamports.
    Sell {
        /// Token base units to sell.
        amount: u64,
        /// Minimum SOL in lamports to receive.
        min_sol_output: u64,
    },
}

impl PumpSwapBuilder {
    /// Build either side: identical 12-account layout, differing only in
    /// discriminator, args payload, and the index-8 program (token program
    /// on buy, associated-token on sell). Order follows the
    /// `solana-pumpfun-swap` skill exactly: `system(7), token(8), rent(9)`
    /// on buy; `system(7), ata-program(8), token(9)` on sell.
    pub fn build_swap_ix(kind: SwapKind, accts: &SwapAccounts<'_>) -> Instruction {
        let mut data = Vec::with_capacity(24);
        match kind {
            SwapKind::Buy { amount, max_sol_cost, track_volume } => {
                data.extend_from_slice(&PUMP_BUY_DISCRIMINATOR);
                PumpBuyArgs { amount, max_sol_cost, track_volume }
                    .serialize(&mut data)
                    .expect("in-memory Borsh encode cannot fail");
            }
            SwapKind::Sell { amount, min_sol_output } => {
                data.extend_from_slice(&PUMP_SELL_DISCRIMINATOR);
                PumpSellArgs { amount, min_sol_output }
                    .serialize(&mut data)
                    .expect("in-memory Borsh encode cannot fail");
            }
        }

        let mut accounts = vec![
            AccountMeta::new_readonly(GLOBAL_ACCOUNT, false),
            AccountMeta::new(*accts.fee_recipient, false),
            AccountMeta::new_readonly(*accts.mint, false),
            AccountMeta::new(*accts.bonding_curve, false),
            AccountMeta::new(*accts.associated_bonding_curve, false),
            AccountMeta::new(*accts.user_ata, false),
            AccountMeta::new(*accts.payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ];
        // Tail layouts, exactly one arm runs (no overwrite-after-push):
        // - legacy buy (no creator):  token program, rent, event, program
        // - legacy sell (no creator): ATA program, token program, event, program
        // - extended buy (creator):   token program, creator vault,
        //   event, program, global-vol, user-vol, fee-config, fee-program
        // - extended sell (creator):  creator vault, token program,
        //   event, program, fee-config, fee-program
        //
        // Verified: legacy pair (skill + IDL + unit lock tests) and the
        // extended BUY (positions 0..16 byte-equal to a frozen mainnet
        // mayhem buy). The extended SELL mirrors the published SDK order
        // but has NO frozen-sample proof yet — treat as experimental
        // (simulation gates it before any funds move).
        match (kind, accts.creator) {
            (SwapKind::Buy { .. }, None) => {
                accounts.extend_from_slice(&[
                    AccountMeta::new_readonly(*accts.token_program, false),
                    AccountMeta::new_readonly(RENT_SYSVAR_ID, false),
                    AccountMeta::new_readonly(EVENT_AUTHORITY, false),
                    AccountMeta::new_readonly(PUMP_PROGRAM_ID, false),
                ]);
            }
            (SwapKind::Sell { .. }, None) => {
                accounts.extend_from_slice(&[
                    AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
                    AccountMeta::new_readonly(*accts.token_program, false),
                    AccountMeta::new_readonly(EVENT_AUTHORITY, false),
                    AccountMeta::new_readonly(PUMP_PROGRAM_ID, false),
                ]);
            }
            (SwapKind::Buy { .. }, Some(creator)) => {
                accounts.extend_from_slice(&[
                    AccountMeta::new_readonly(*accts.token_program, false),
                    AccountMeta::new(creator_vault_pda(creator), false),
                    AccountMeta::new_readonly(EVENT_AUTHORITY, false),
                    AccountMeta::new_readonly(PUMP_PROGRAM_ID, false),
                    AccountMeta::new(GLOBAL_VOLUME_ACCUMULATOR, false),
                    AccountMeta::new(user_volume_accumulator_pda(accts.payer), false),
                    AccountMeta::new_readonly(FEE_CONFIG_ACCOUNT, false),
                    AccountMeta::new_readonly(FEE_CONFIG_PROGRAM, false),
                ]);
            }
            (SwapKind::Sell { .. }, Some(creator)) => {
                accounts.extend_from_slice(&[
                    AccountMeta::new(creator_vault_pda(creator), false),
                    AccountMeta::new_readonly(*accts.token_program, false),
                    AccountMeta::new_readonly(EVENT_AUTHORITY, false),
                    AccountMeta::new_readonly(PUMP_PROGRAM_ID, false),
                    AccountMeta::new_readonly(FEE_CONFIG_ACCOUNT, false),
                    AccountMeta::new_readonly(FEE_CONFIG_PROGRAM, false),
                ]);
            }
        }
        // Mayhem buyback tail: buyback-enforcing curves reject the
        // bare 16-account prefix with 6062 (proven live off a fresh
        // launch), so append the reference-resolved [16]/[17] when
        // present (buys on mayhem curves only — the arm above already
        // requires a creator). Without routing the 16-account prefix
        // still builds (the 18-account byte-proof shares this body),
        // but dispatch never sends it: no reference fails closed
        // upstream in [`SwapExecutor::preflight_buy`].
        if matches!(kind, SwapKind::Buy { .. })
            && accts.creator.is_some()
            && let Some(routing) = &accts.mayhem_routing
        {
            accounts.extend_from_slice(&[
                AccountMeta::new(*routing.unknown_16, false),
                AccountMeta::new(*routing.pfee_vault, false),
            ]);
        }

        Instruction { program_id: PUMP_PROGRAM_ID, accounts, data }
    }

    /// v2 sell instruction: the full 26-account mayhem shape for exits
    /// on v2 curves (legacy sells are rejected there — without this,
    /// positions opened on v2 curves cannot close).
    ///
    /// Account order and mutability mirror the frozen `sell_v2_mainnet`
    /// sample exactly (see [`SellV2Accounts`] for provenance). Derived
    /// addresses (user/fee/creator/curve/uvol quote ATAs, creator vault,
    /// user-vol accumulator) are computed inside; opaque routing accounts
    /// come from the caller (curve state or a same-curve reference sell).
    pub fn build_sell_v2_ix(
        amount: u64,
        min_sol_output: u64,
        accts: &SellV2Accounts<'_>,
    ) -> Instruction {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&PUMP_SELL_V2_DISCRIMINATOR);
        PumpSellV2Args { amount, min_sol_output }
            .serialize(&mut data)
            .expect("in-memory Borsh encode cannot fail");

        let creator_vault = creator_vault_pda(accts.creator);
        let user_vol = user_volume_accumulator_pda(accts.payer);
        let quote_ata = |owner: &Pubkey| {
            user_ata_with_program(owner, accts.quote_mint, accts.quote_token_program)
        };
        let accounts = vec![
            AccountMeta::new_readonly(GLOBAL_ACCOUNT, false),                    // [0]
            AccountMeta::new_readonly(*accts.mint, false),                       // [1]
            AccountMeta::new_readonly(*accts.quote_mint, false),                 // [2]
            AccountMeta::new_readonly(*accts.base_token_program, false),         // [3]
            AccountMeta::new_readonly(*accts.quote_token_program, false),        // [4]
            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),       // [5]
            AccountMeta::new(*accts.fee_recipient, false),                       // [6]
            AccountMeta::new(quote_ata(accts.fee_recipient), false),             // [7]
            AccountMeta::new(*accts.pfee_vault, false),                          // [8]
            AccountMeta::new(*accts.pfee_quote_account, false),                  // [9]
            AccountMeta::new(*accts.bonding_curve, false),                       // [10]
            AccountMeta::new(*accts.associated_bonding_curve, false),            // [11]
            AccountMeta::new(quote_ata(accts.bonding_curve), false),             // [12]
            AccountMeta::new(*accts.payer, true),                                // [13]
            AccountMeta::new(
                user_ata_with_program(accts.payer, accts.mint, accts.base_token_program),
                false,
            ), // [14]
            AccountMeta::new(quote_ata(accts.payer), false),                     // [15]
            AccountMeta::new(creator_vault, false),                              // [16]
            AccountMeta::new(quote_ata(&creator_vault), false),                  // [17]
            AccountMeta::new_readonly(sharing_config_pda(accts.mint), false),    // [18]
            AccountMeta::new(user_vol, false),                                   // [19]
            AccountMeta::new(quote_ata(&user_vol), false),                       // [20]
            AccountMeta::new_readonly(FEE_CONFIG_ACCOUNT, false),                // [21]
            AccountMeta::new_readonly(FEE_CONFIG_PROGRAM, false),                // [22]
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),                 // [23]
            AccountMeta::new_readonly(EVENT_AUTHORITY, false),                   // [24]
            AccountMeta::new_readonly(PUMP_PROGRAM_ID, false),                   // [25]
        ];
        debug_assert_eq!(accounts.len(), 26);
        Instruction { program_id: PUMP_PROGRAM_ID, accounts, data }
    }

    /// v2 buy instruction: the full 27-account mayhem shape for entries
    /// on v2 curves (the 18-account mayhem buy fails there — frozen
    /// failed sample — and legacy 12-account buys fail with 6062).
    ///
    /// Account order mirrors [`PumpSwapBuilder::build_sell_v2_ix`] with
    /// the global volume accumulator inserted at [19] (buys track volume
    /// globally; sells don't). Derived addresses (creator vault [16],
    /// user-vol accumulator [20], all six base/quote ATAs) are computed
    /// inside and verified byte-exact against both frozen `buy_v2`
    /// samples; opaque routing comes from the caller (a same-curve
    /// reference buy_v2). Writability mirrors the proven `sell_v2` set
    /// plus writable [19]/[20]/[21] — preflight simulation gates every
    /// build (the 27-shape sim probe is the gate that proves the flags).
    pub fn build_buy_v2_ix(
        amount: u64,
        max_sol_cost: u64,
        accts: &BuyV2Accounts<'_>,
    ) -> Instruction {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&PUMP_BUY_V2_DISCRIMINATOR);
        PumpBuyV2Args { amount, max_sol_cost }
            .serialize(&mut data)
            .expect("in-memory Borsh encode cannot fail");

        let creator_vault = creator_vault_pda(accts.creator);
        let user_vol = user_volume_accumulator_pda(accts.payer);
        let quote_ata = |owner: &Pubkey| {
            user_ata_with_program(owner, accts.quote_mint, accts.quote_token_program)
        };
        let accounts = vec![
            AccountMeta::new_readonly(GLOBAL_ACCOUNT, false),                    // [0]
            AccountMeta::new_readonly(*accts.mint, false),                       // [1]
            AccountMeta::new_readonly(*accts.quote_mint, false),                 // [2]
            AccountMeta::new_readonly(*accts.base_token_program, false),         // [3]
            AccountMeta::new_readonly(*accts.quote_token_program, false),        // [4]
            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),       // [5]
            AccountMeta::new(*accts.fee_recipient, false),                       // [6]
            AccountMeta::new(quote_ata(accts.fee_recipient), false),             // [7]
            AccountMeta::new(*accts.pfee_vault, false),                          // [8]
            AccountMeta::new(*accts.pfee_quote_account, false),                  // [9]
            AccountMeta::new(*accts.bonding_curve, false),                       // [10]
            AccountMeta::new(*accts.associated_bonding_curve, false),            // [11]
            AccountMeta::new(quote_ata(accts.bonding_curve), false),             // [12]
            AccountMeta::new(*accts.payer, true),                                // [13]
            AccountMeta::new(
                user_ata_with_program(accts.payer, accts.mint, accts.base_token_program),
                false,
            ), // [14]
            AccountMeta::new(quote_ata(accts.payer), false),                     // [15]
            AccountMeta::new(creator_vault, false),                              // [16]
            AccountMeta::new(quote_ata(&creator_vault), false),                  // [17]
            AccountMeta::new_readonly(sharing_config_pda(accts.mint), false),    // [18]
            AccountMeta::new(GLOBAL_VOLUME_ACCUMULATOR, false),                  // [19]
            AccountMeta::new(user_vol, false),                                   // [20]
            AccountMeta::new(quote_ata(&user_vol), false),                       // [21]
            AccountMeta::new_readonly(FEE_CONFIG_ACCOUNT, false),                // [22]
            AccountMeta::new_readonly(FEE_CONFIG_PROGRAM, false),                // [23]
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),                 // [24]
            AccountMeta::new_readonly(EVENT_AUTHORITY, false),                   // [25]
            AccountMeta::new_readonly(PUMP_PROGRAM_ID, false),                   // [26]
        ];
        debug_assert_eq!(accounts.len(), 27);
        Instruction { program_id: PUMP_PROGRAM_ID, accounts, data }
    }

    /// v3 buy instruction: the 17-account shape for entries on any curve
    /// (mayhem included; cashback coins excepted — they fail v3 with
    /// `CashbackCoinNotSupported` and must use v2). Fees stay on the
    /// curve, so neither a fee recipient nor a same-curve reference is
    /// needed — only the buyback pick plus derived addresses.
    /// Writability mirrors the v2 convention minus the removed
    /// accounts; preflight simulation gates every build (the 17-shape
    /// sim probe is the gate that proves the flags).
    pub fn build_buy_v3_ix(
        amount: u64,
        max_sol_cost: u64,
        accts: &BuyV3Accounts<'_>,
    ) -> Instruction {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&PUMP_BUY_V3_DISCRIMINATOR);
        PumpBuyV3Args { amount, max_sol_cost }
            .serialize(&mut data)
            .expect("in-memory Borsh encode cannot fail");
        Self::build_v3_ix(data, accts.payer, accts.mint, accts.quote_mint, accts.bonding_curve,
            accts.associated_bonding_curve, accts.base_token_program, accts.quote_token_program,
            accts.buyback_recipient)
    }

    /// v3 sell instruction: the 17-account shape for exits on any curve
    /// (same cashback exception as [`PumpSwapBuilder::build_buy_v3_ix`]).
    pub fn build_sell_v3_ix(
        amount: u64,
        min_sol_output: u64,
        accts: &SellV3Accounts<'_>,
    ) -> Instruction {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&PUMP_SELL_V3_DISCRIMINATOR);
        PumpSellV3Args { amount, min_sol_output }
            .serialize(&mut data)
            .expect("in-memory Borsh encode cannot fail");
        Self::build_v3_ix(data, accts.payer, accts.mint, accts.quote_mint, accts.bonding_curve,
            accts.associated_bonding_curve, accts.base_token_program, accts.quote_token_program,
            accts.buyback_recipient)
    }

    /// Shared 17-account v3 body: `data` selects the side, everything
    /// else is positionally identical across `buy_v3`/`sell_v3`.
    #[allow(clippy::too_many_arguments)]
    fn build_v3_ix(
        data: Vec<u8>,
        payer: &Pubkey,
        mint: &Pubkey,
        quote_mint: &Pubkey,
        bonding_curve: &Pubkey,
        associated_bonding_curve: &Pubkey,
        base_token_program: &Pubkey,
        quote_token_program: &Pubkey,
        buyback_recipient: &Pubkey,
    ) -> Instruction {
        let user_vol = user_volume_accumulator_pda(payer);
        let base_ata = || user_ata_with_program(payer, mint, base_token_program);
        let quote_ata = |owner: &Pubkey| user_ata_with_program(owner, quote_mint, quote_token_program);
        let accounts = vec![
            AccountMeta::new_readonly(GLOBAL_ACCOUNT, false),                    // [0]
            AccountMeta::new_readonly(*mint, false),                             // [1]
            AccountMeta::new_readonly(*quote_mint, false),                       // [2]
            AccountMeta::new_readonly(*base_token_program, false),               // [3]
            AccountMeta::new_readonly(*quote_token_program, false),              // [4]
            AccountMeta::new(*bonding_curve, false),                             // [5]
            AccountMeta::new(*associated_bonding_curve, false),                  // [6]
            AccountMeta::new(quote_ata(bonding_curve), false),                   // [7]
            AccountMeta::new(*payer, true),                                      // [8]
            AccountMeta::new(base_ata(), false),                                 // [9]
            AccountMeta::new(quote_ata(payer), false),                           // [10]
            AccountMeta::new(user_vol, false),                                   // [11]
            AccountMeta::new_readonly(FEE_CONFIG_ACCOUNT, false),                // [12]
            AccountMeta::new(*buyback_recipient, false),                         // [13]
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),                 // [14]
            AccountMeta::new_readonly(EVENT_AUTHORITY, false),                   // [15]
            AccountMeta::new_readonly(PUMP_PROGRAM_ID, false),                   // [16]
        ];
        debug_assert_eq!(accounts.len(), 17);
        Instruction { program_id: PUMP_PROGRAM_ID, accounts, data }
    }

    /// v3 exact-quote buy instruction: spend exactly `spendable_quote_in`
    /// for at least `min_tokens_out` tokens (same 17 accounts as
    /// [`PumpSwapBuilder::build_buy_v3_ix`]). Suited to fixed-notional
    /// sniping (exact spend, floor on proceeds) — enable in dispatch
    /// only after its frozen-sample byte test + sim probe turn green.
    pub fn build_buy_exact_quote_in_v3_ix(
        spendable_quote_in: u64,
        min_tokens_out: u64,
        accts: &BuyV3Accounts<'_>,
    ) -> Instruction {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&PUMP_BUY_EXACT_QUOTE_IN_V3_DISCRIMINATOR);
        PumpBuyExactQuoteInV3Args { spendable_quote_in, min_tokens_out }
            .serialize(&mut data)
            .expect("in-memory Borsh encode cannot fail");
        Self::build_v3_ix(data, accts.payer, accts.mint, accts.quote_mint, accts.bonding_curve,
            accts.associated_bonding_curve, accts.base_token_program, accts.quote_token_program,
            accts.buyback_recipient)
    }
}

/// Derive the payer's associated token account for a mint.
///
/// Cross-crate conversion via raw bytes keeps this independent of the
/// `spl-associated-token-account` crate's internal pubkey version.
pub fn user_ata(payer: &Pubkey, mint: &Pubkey) -> Pubkey {
    let theirs = spl_associated_token_account::get_associated_token_address(
        &pubkey_into_theirs(payer),
        &pubkey_into_theirs(mint),
    );
    Pubkey::new_from_array(theirs.to_bytes())
}

/// Associated token address bound to an explicit token program.
/// REQUIRED for Token-2022 mints: the ATA seeds are
/// `[owner, token_program, mint]`, so the classic helper derives a
/// DIFFERENT (wrong) address for 2022 mints — creating or trading
/// through it fails on-chain (verified live: `IncorrectProgramId` in
/// the ATA program's classic-token CPI). Always prefer this over
/// [`user_ata`] when the mint owner is known (it always is at
/// preflight: read it off the mint account).
pub fn user_ata_with_program(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    let theirs = spl_associated_token_account::get_associated_token_address_with_program_id(
        &pubkey_into_theirs(owner),
        &pubkey_into_theirs(mint),
        &pubkey_into_theirs(token_program),
    );
    Pubkey::new_from_array(theirs.to_bytes())
}

fn pubkey_into_theirs(key: &Pubkey) -> Pubkey {
    Pubkey::new_from_array(key.to_bytes())
}

/// Compute Budget program (canonical, stable across SDK versions).
pub const COMPUTE_BUDGET_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");

/// Build the compute-budget prefix: `SetComputeUnitLimit` then
/// `SetComputeUnitPrice` (runtime convention: limit before price).
/// Zero values are skipped individually, so a default configuration
/// yields an empty prefix and byte-identical transactions.
pub fn compute_budget_ixs(unit_limit: u32, unit_price_micro_lamports: u64) -> Vec<Instruction> {
    let mut out = Vec::with_capacity(2);
    if unit_limit > 0 {
        let mut data = vec![2u8];
        data.extend_from_slice(&unit_limit.to_le_bytes());
        out.push(Instruction {
            program_id: COMPUTE_BUDGET_PROGRAM_ID,
            accounts: vec![],
            data,
        });
    }
    if unit_price_micro_lamports > 0 {
        let mut data = vec![3u8];
        data.extend_from_slice(&unit_price_micro_lamports.to_le_bytes());
        out.push(Instruction {
            program_id: COMPUTE_BUDGET_PROGRAM_ID,
            accounts: vec![],
            data,
        });
    }
    out
}
/// Build an idempotent create-associated-token-account instruction
/// (canonical 7-account layout, `CreateIdempotent` discriminator).
/// Prepended when the user's ATA for a mint does not exist yet —
/// otherwise the swap fails on-chain after burning fees. Idempotent
/// form doubles as race insurance: even if the ATA lands between the
/// existence check and execution, the prefix succeeds silently.
pub fn build_create_ata_ix(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    ata: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(*ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            // Per-mint program: a Token-2022 mint created through the
            // classic program fails inside its InitializeAccount CPI
            // (`IncorrectProgramId`, verified live).
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new_readonly(RENT_SYSVAR_ID, false),
        ],
        // Idempotent variant (`1`, not `0`): safe to prepend even when
        // the ATA already exists (no duplicate-create failure) and
        // immune to check-vs-land races between preflight and execution.
        data: vec![1],
    }
}

/// Assert our address constants match the SPL crates (drift detector).
pub fn spl_ids_match() -> bool {
    TOKEN_PROGRAM_ID.to_bytes() == spl_token::ID.to_bytes()
        && ASSOCIATED_TOKEN_PROGRAM_ID.to_bytes() == spl_associated_token_account::ID.to_bytes()
}

/// Parse helper for tests and plumbing.
pub fn parse_pubkey(s: &str) -> Result<Pubkey, solana_pubkey::ParsePubkeyError> {
    Pubkey::from_str(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> (Pubkey, Pubkey, Pubkey, Pubkey, Pubkey) {
        (
            Pubkey::new_from_array([1; 32]),
            Pubkey::new_from_array([2; 32]),
            Pubkey::new_from_array([3; 32]),
            Pubkey::new_from_array([4; 32]),
            Pubkey::new_from_array([5; 32]),
        )
    }

    #[test]
    fn program_addresses_match_skill() {
        assert_eq!(
            PUMP_PROGRAM_ID.to_string(),
            "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"
        );
        assert_eq!(
            FEE_RECIPIENT.to_string(),
            "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM"
        );
        assert_eq!(
            FEE_RECIPIENT.to_string(),
            "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM"
        );
        assert_eq!(
            GLOBAL_ACCOUNT.to_string(),
            "4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf"
        );
        assert_eq!(
            EVENT_AUTHORITY.to_string(),
            "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1"
        );
        assert!(spl_ids_match());
    }

    #[test]
    fn sell_v2_matches_frozen_sample() {
        // Byte-equality against the frozen mainnet sell_v2 (26 accounts):
        // our builder must reproduce every position with exact mutability
        // (flags verified via the tx's `jsonParsed` account table) plus
        // the payload. The derived addresses double as independent
        // anchors: creator vault ([16]) and user-vol accumulator ([19])
        // are recomputed from seeds, not copied.
        use std::collections::HashSet;
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../listener/tests/fixtures/sell_v2_mainnet.json"
        ))
        .expect("fixture");
        let result = &raw["result"];
        let msg = &result["transaction"]["message"];
        let mut keys: Vec<String> =
            serde_json::from_value(msg["accountKeys"].clone()).expect("keys");
        // Address-table extension (resolve_keys order: writable first).
        if let Some(loaded) = result["meta"]["loadedAddresses"].as_object() {
            for table in ["writable", "readonly"] {
                for addr in loaded
                    .get(table)
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                {
                    keys.push(addr.as_str().expect("addr").to_string());
                }
            }
        }
        let pump = PUMP_PROGRAM_ID.to_string();
        let pi = keys.iter().position(|k| k == &pump).expect("pump key");
        let mut found = None;
        for ix in msg["instructions"].as_array().expect("ixs") {
            if ix["programIdIndex"].as_u64() != Some(pi as u64) {
                continue;
            }
            let data = bs58::decode(ix["data"].as_str().expect("data"))
                .into_vec()
                .expect("b58");
            if data.starts_with(&PUMP_SELL_V2_DISCRIMINATOR) && data.len() == 24 {
                let accounts: Vec<usize> =
                    serde_json::from_value(ix["accounts"].clone()).expect("accts");
                found = Some((accounts, data));
                break;
            }
        }
        let (idx, data) = found.expect("sell_v2 in fixture");
        assert_eq!(idx.len(), 26);
        let key = |i: usize| Pubkey::from_str(&keys[idx[i]]).expect("pubkey");
        let payer = key(13);
        // Creator from the RugCheck report for the fixture mint (also
        // readable at curve-account offset 49..81 on-chain).
        let creator =
            Pubkey::from_str("9nkAbQ5QUY4xH9Te7niPXFiPqsCZTfN5nTgrmTWQ1FYF").expect("creator");
        assert_eq!(creator_vault_pda(&creator), key(16));
        assert_eq!(user_volume_accumulator_pda(&payer), key(19));
        let amount = u64::from_le_bytes(data[8..16].try_into().expect("u64"));
        let min_sol_output = u64::from_le_bytes(data[16..24].try_into().expect("u64"));
        assert_eq!(amount, 3_060_466_246_688);
        let built = PumpSwapBuilder::build_sell_v2_ix(
            amount,
            min_sol_output,
            &SellV2Accounts {
                payer: &payer,
                mint: &key(1),
                quote_mint: &key(2),
                bonding_curve: &key(10),
                associated_bonding_curve: &key(11),
                fee_recipient: &key(6),
                base_token_program: &key(3),
                quote_token_program: &key(4),
                creator: &creator,
                pfee_vault: &key(8),
                pfee_quote_account: &key(9),
            },
        );
        assert_eq!(sharing_config_pda(&key(1)), key(18));
        assert_eq!(built.accounts.len(), 26);
        // Writable set from the jsonParsed flags (13 is the only signer).
        let writable: HashSet<usize> = [6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 19, 20]
            .into_iter()
            .collect();
        for (i, meta) in built.accounts.iter().enumerate() {
            assert_eq!(meta.pubkey, key(i), "position {i}");
            let expect = if i == 13 {
                AccountMeta::new(key(i), true)
            } else if writable.contains(&i) {
                AccountMeta::new(key(i), false)
            } else {
                AccountMeta::new_readonly(key(i), false)
            };
            assert_eq!(*meta, expect, "flags {i}");
        }
        assert_eq!(built.data, data);
    }

    #[test]
    fn buy_v2_matches_frozen_samples() {
        // Byte-equality against TWO real mainnet `buy_v2` fills (27
        // accounts each): our builder must reproduce every position plus
        // the payload. Derived addresses double as independent anchors:
        // creator vault ([16]) recomputed from the create_v2 author
        // (position 5, launcher in both samples) and user-vol accumulator
        // ([20]) from the payer — neither copied. Reference-sourced
        // routing ([2]/[6]/[8]/[9]/[18]) is lifted from the fixture, like
        // production lifts it from a same-curve reference buy. Flags
        // follow the proven `sell_v2` set plus writable global-vol [19]:
        // the 27-shape sim probe (not this test) proves the flags.
        use std::collections::HashSet;
        let fixtures: [(&str, &str, u64); 2] = [
            (
                include_str!("../../listener/tests/fixtures/create_v2_mainnet.json"),
                "../../listener/tests/fixtures/create_v2_mainnet.json",
                3_485_709_524_202u64,
            ),
            (
                include_str!("../../listener/tests/fixtures/buy_v2_second_mainnet.json"),
                "../../listener/tests/fixtures/buy_v2_second_mainnet.json",
                8_671_591_758_466u64,
            ),
        ];
        for (contents, path, want_amount) in fixtures {
            let raw: serde_json::Value =
                serde_json::from_str(contents).expect("fixture");
            let result = &raw["result"];
            let msg = &result["transaction"]["message"];
            let mut keys: Vec<String> =
                serde_json::from_value(msg["accountKeys"].clone()).expect("keys");
            if let Some(loaded) = result["meta"]["loadedAddresses"].as_object() {
                for table in ["writable", "readonly"] {
                    for addr in loaded
                        .get(table)
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default()
                    {
                        keys.push(addr.as_str().expect("addr").to_string());
                    }
                }
            }
            let pump = PUMP_PROGRAM_ID.to_string();
            let pi = keys.iter().position(|k| k == &pump).expect("pump key");
            let ixs = msg["instructions"].as_array().expect("ixs");
            // Creator from the same-tx `create_v2` author (position 5):
            // the vault match below proves the provenance.
            let mut creator = None;
            for ix in ixs {
                if ix["programIdIndex"].as_u64() != Some(pi as u64) {
                    continue;
                }
                let data = bs58::decode(ix["data"].as_str().expect("data"))
                    .into_vec()
                    .expect("b58");
                if !data.starts_with(&PUMP_CREATE_V2_DISCRIMINATOR) {
                    continue;
                }
                let accounts: Vec<usize> =
                    serde_json::from_value(ix["accounts"].clone()).expect("accts");
                creator = Some(
                    Pubkey::from_str(&keys[accounts[5]].clone()).expect("creator"),
                );
                break;
            }
            let creator = creator.expect("create_v2 in fixture");
            let mut found = None;
            for ix in ixs {
                if ix["programIdIndex"].as_u64() != Some(pi as u64) {
                    continue;
                }
                let data = bs58::decode(ix["data"].as_str().expect("data"))
                    .into_vec()
                    .expect("b58");
                if data.starts_with(&PUMP_BUY_V2_DISCRIMINATOR) && data.len() == 24 {
                    let accounts: Vec<usize> =
                        serde_json::from_value(ix["accounts"].clone()).expect("accts");
                    found = Some((accounts, data));
                    break;
                }
            }
            let (idx, data) = found.expect("buy_v2 in fixture");
            assert_eq!(idx.len(), 27, "{path}");
            let key = |i: usize| Pubkey::from_str(&keys[idx[i]]).expect("pubkey");
            assert_eq!(creator_vault_pda(&creator), key(16), "{path} creator vault");
            assert_eq!(
                user_volume_accumulator_pda(&key(13)),
                key(20),
                "{path} user-vol"
            );
            let amount = u64::from_le_bytes(data[8..16].try_into().expect("u64"));
            let max_sol_cost = u64::from_le_bytes(data[16..24].try_into().expect("u64"));
            assert_eq!(amount, want_amount, "{path}");
            let built = PumpSwapBuilder::build_buy_v2_ix(
                amount,
                max_sol_cost,
                &BuyV2Accounts {
                    payer: &key(13),
                    mint: &key(1),
                    quote_mint: &key(2),
                    bonding_curve: &key(10),
                    associated_bonding_curve: &key(11),
                    fee_recipient: &key(6),
                    base_token_program: &key(3),
                    quote_token_program: &key(4),
                    creator: &creator,
                    pfee_vault: &key(8),
                    pfee_quote_account: &key(9),
                },
            );
            assert_eq!(sharing_config_pda(&key(1)), key(18), "{path}");
            assert_eq!(built.accounts.len(), 27, "{path}");
            // Inferred writable set (sell_v2 set + [19]/[20]/[21];
            // position 13 is the only signer).
            let writable: HashSet<usize> = [
                6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 19, 20, 21,
            ]
            .into_iter()
            .collect();
            for (i, meta) in built.accounts.iter().enumerate() {
                assert_eq!(meta.pubkey, key(i), "{path} position {i}");
                let expect = if i == 13 {
                    AccountMeta::new(key(i), true)
                } else if writable.contains(&i) {
                    AccountMeta::new(key(i), false)
                } else {
                    AccountMeta::new_readonly(key(i), false)
                };
                assert_eq!(*meta, expect, "{path} flags {i}");
            }
            assert_eq!(built.data, data, "{path}");
        }
    }

    #[test]
    fn sell_v3_matches_frozen_sample() {
        // Byte-equality against a real mainnet `sell_v3` fill (17
        // accounts): our builder must reproduce every position plus the
        // payload. Derived addresses double as independent anchors:
        // user-vol accumulator ([11]) from the payer plus all four
        // base/quote ATAs ([6]/[7]/[9]/[10]) recomputed, not copied. The
        // buyback recipient ([13]) must be a member of the published
        // 8-list — asserted explicitly (both frozen v3 fills use index
        // 1). Flags follow the v2 convention minus the removed
        // accounts; the 17-shape sim probe (not this test) proves them.
        use std::collections::HashSet;
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../listener/tests/fixtures/sell_v3_mainnet.json"
        ))
        .expect("fixture");
        // Shape: bare getTransaction result (no RPC envelope), like the
        // mayhem fixtures.
        let result = &raw;
        // Landed transaction only: a failed fill proves nothing.
        assert!(
            result
                .get("meta")
                .and_then(|m| m.get("err"))
                .is_some_and(|e| e.is_null()),
            "success fixture must carry a landed transaction"
        );
        let msg = &result["transaction"]["message"];
        let mut keys: Vec<String> =
            serde_json::from_value(msg["accountKeys"].clone()).expect("keys");
        if let Some(loaded) = result["meta"]["loadedAddresses"].as_object() {
            for table in ["writable", "readonly"] {
                for addr in loaded
                    .get(table)
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                {
                    keys.push(addr.as_str().expect("addr").to_string());
                }
            }
        }
        let pump = PUMP_PROGRAM_ID.to_string();
        let pi = keys.iter().position(|k| k == &pump).expect("pump key");
        let mut found = None;
        for ix in msg["instructions"].as_array().expect("ixs") {
            if ix["programIdIndex"].as_u64() != Some(pi as u64) {
                continue;
            }
            let data = bs58::decode(ix["data"].as_str().expect("data"))
                .into_vec()
                .expect("b58");
            if data.starts_with(&PUMP_SELL_V3_DISCRIMINATOR) && data.len() == 24 {
                let accounts: Vec<usize> =
                    serde_json::from_value(ix["accounts"].clone()).expect("accts");
                found = Some((accounts, data));
                break;
            }
        }
        let (idx, data) = found.expect("sell_v3 in fixture");
        assert_eq!(idx.len(), 17);
        let key = |i: usize| Pubkey::from_str(&keys[idx[i]]).expect("pubkey");
        assert_eq!(user_volume_accumulator_pda(&key(8)), key(11));
        assert!(
            BUYBACK_FEE_RECIPIENTS.contains(&key(13)),
            "buyback recipient must come from the published 8-list"
        );
        let amount = u64::from_le_bytes(data[8..16].try_into().expect("u64"));
        let min_sol_output = u64::from_le_bytes(data[16..24].try_into().expect("u64"));
        assert_eq!(amount, 30_952_762_059);
        let built = PumpSwapBuilder::build_sell_v3_ix(
            amount,
            min_sol_output,
            &SellV3Accounts {
                payer: &key(8),
                mint: &key(1),
                quote_mint: &key(2),
                bonding_curve: &key(5),
                associated_bonding_curve: &key(6),
                base_token_program: &key(3),
                quote_token_program: &key(4),
                buyback_recipient: &key(13),
            },
        );
        assert_eq!(built.accounts.len(), 17);
        // Inferred writable set (positions 5–11 + 13; 8 is the signer).
        let writable: HashSet<usize> = [5, 6, 7, 8, 9, 10, 11, 13].into_iter().collect();
        for (i, meta) in built.accounts.iter().enumerate() {
            assert_eq!(meta.pubkey, key(i), "position {i}");
            let expect = if i == 8 {
                AccountMeta::new(key(i), true)
            } else if writable.contains(&i) {
                AccountMeta::new(key(i), false)
            } else {
                AccountMeta::new_readonly(key(i), false)
            };
            assert_eq!(*meta, expect, "flags {i}");
        }
        assert_eq!(built.data, data);
    }

    #[test]
    fn buy_v3_matches_frozen_sample() {
        // Byte-equality against a real mainnet `buy_v3` fill (17
        // accounts, no `partial_fill` tail): our builder must reproduce
        // every position plus the payload. Same shared 17-account body
        // as `sell_v3` — user-vol accumulator ([11]) plus all four
        // base/quote ATAs ([6]/[7]/[9]/[10]) recomputed, buyback ([13])
        // pinned to the published 8-list. Flags are the same inferred
        // set as `sell_v3`; the 17-shape sim probe proves them.
        use std::collections::HashSet;
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../listener/tests/fixtures/buy_v3_mainnet.json"
        ))
        .expect("fixture");
        // Shape: bare getTransaction result (no RPC envelope), like the
        // mayhem fixtures.
        let result = &raw;
        // Landed transaction only: a failed fill proves nothing.
        assert!(
            result
                .get("meta")
                .and_then(|m| m.get("err"))
                .is_some_and(|e| e.is_null()),
            "success fixture must carry a landed transaction"
        );
        let msg = &result["transaction"]["message"];
        let mut keys: Vec<String> =
            serde_json::from_value(msg["accountKeys"].clone()).expect("keys");
        if let Some(loaded) = result["meta"]["loadedAddresses"].as_object() {
            for table in ["writable", "readonly"] {
                for addr in loaded
                    .get(table)
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                {
                    keys.push(addr.as_str().expect("addr").to_string());
                }
            }
        }
        let pump = PUMP_PROGRAM_ID.to_string();
        let pi = keys.iter().position(|k| k == &pump).expect("pump key");
        let mut found = None;
        for ix in msg["instructions"].as_array().expect("ixs") {
            if ix["programIdIndex"].as_u64() != Some(pi as u64) {
                continue;
            }
            let data = bs58::decode(ix["data"].as_str().expect("data"))
                .into_vec()
                .expect("b58");
            if data.starts_with(&PUMP_BUY_V3_DISCRIMINATOR) && data.len() == 24 {
                let accounts: Vec<usize> =
                    serde_json::from_value(ix["accounts"].clone()).expect("accts");
                found = Some((accounts, data));
                break;
            }
        }
        let (idx, data) = found.expect("buy_v3 in fixture");
        assert_eq!(idx.len(), 17);
        let key = |i: usize| Pubkey::from_str(&keys[idx[i]]).expect("pubkey");
        assert_eq!(user_volume_accumulator_pda(&key(8)), key(11));
        assert!(
            BUYBACK_FEE_RECIPIENTS.contains(&key(13)),
            "buyback recipient must come from the published 8-list"
        );
        let amount = u64::from_le_bytes(data[8..16].try_into().expect("u64"));
        let max_sol_cost = u64::from_le_bytes(data[16..24].try_into().expect("u64"));
        assert_eq!(amount, 34_612_903_225_806);
        assert_eq!(max_sol_cost, 1_100_000_000);
        let built = PumpSwapBuilder::build_buy_v3_ix(
            amount,
            max_sol_cost,
            &BuyV3Accounts {
                payer: &key(8),
                mint: &key(1),
                quote_mint: &key(2),
                bonding_curve: &key(5),
                associated_bonding_curve: &key(6),
                base_token_program: &key(3),
                quote_token_program: &key(4),
                buyback_recipient: &key(13),
            },
        );
        assert_eq!(built.accounts.len(), 17);
        // Same inferred writable set as `sell_v3` (8 is the signer).
        let writable: HashSet<usize> = [5, 6, 7, 8, 9, 10, 11, 13].into_iter().collect();
        for (i, meta) in built.accounts.iter().enumerate() {
            assert_eq!(meta.pubkey, key(i), "position {i}");
            let expect = if i == 8 {
                AccountMeta::new(key(i), true)
            } else if writable.contains(&i) {
                AccountMeta::new(key(i), false)
            } else {
                AccountMeta::new_readonly(key(i), false)
            };
            assert_eq!(*meta, expect, "flags {i}");
        }
        assert_eq!(built.data, data);
    }

    #[test]
    fn buy_exact_quote_in_v3_matches_frozen_sample() {
        // Byte-equality against a real mainnet `buy_exact_quote_in_v3`
        // fill (17 accounts): same shared body as `buy_v3`/`sell_v3`,
        // payload (spend, min_out). Spend was exactly 1 SOL, floor
        // 12_211_736_678_104. Derivation anchors mirror the `buy_v3`
        // test (uvol PDA, 8-list buyback, four ATAs).
        use std::collections::HashSet;
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../listener/tests/fixtures/buy_exact_quote_in_v3_mainnet.json"
        ))
        .expect("fixture");
        // Shape: bare getTransaction result (no RPC envelope).
        let result = &raw;
        assert!(
            result
                .get("meta")
                .and_then(|m| m.get("err"))
                .is_some_and(|e| e.is_null()),
            "success fixture must carry a landed transaction"
        );
        let msg = &result["transaction"]["message"];
        let mut keys: Vec<String> =
            serde_json::from_value(msg["accountKeys"].clone()).expect("keys");
        if let Some(loaded) = result["meta"]["loadedAddresses"].as_object() {
            for table in ["writable", "readonly"] {
                for addr in loaded
                    .get(table)
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                {
                    keys.push(addr.as_str().expect("addr").to_string());
                }
            }
        }
        let pump = PUMP_PROGRAM_ID.to_string();
        let pi = keys.iter().position(|k| k == &pump).expect("pump key");
        let mut found = None;
        for ix in msg["instructions"].as_array().expect("ixs") {
            if ix["programIdIndex"].as_u64() != Some(pi as u64) {
                continue;
            }
            let data = bs58::decode(ix["data"].as_str().expect("data"))
                .into_vec()
                .expect("b58");
            if data.starts_with(&PUMP_BUY_EXACT_QUOTE_IN_V3_DISCRIMINATOR) && data.len() == 24
            {
                let accounts: Vec<usize> =
                    serde_json::from_value(ix["accounts"].clone()).expect("accts");
                found = Some((accounts, data));
                break;
            }
        }
        let (idx, data) = found.expect("buy_exact_quote_in_v3 in fixture");
        assert_eq!(idx.len(), 17);
        let key = |i: usize| Pubkey::from_str(&keys[idx[i]]).expect("pubkey");
        assert_eq!(user_volume_accumulator_pda(&key(8)), key(11));
        assert!(
            BUYBACK_FEE_RECIPIENTS.contains(&key(13)),
            "buyback recipient must come from the published 8-list"
        );
        let spend = u64::from_le_bytes(data[8..16].try_into().expect("u64"));
        let min_out = u64::from_le_bytes(data[16..24].try_into().expect("u64"));
        assert_eq!(spend, 1_000_000_000);
        assert_eq!(min_out, 12_211_736_678_104);
        let built = PumpSwapBuilder::build_buy_exact_quote_in_v3_ix(
            spend,
            min_out,
            &BuyV3Accounts {
                payer: &key(8),
                mint: &key(1),
                quote_mint: &key(2),
                bonding_curve: &key(5),
                associated_bonding_curve: &key(6),
                base_token_program: &key(3),
                quote_token_program: &key(4),
                buyback_recipient: &key(13),
            },
        );
        assert_eq!(built.accounts.len(), 17);
        // Same inferred set as the other v3 sides (8 is the signer).
        let writable: HashSet<usize> = [5, 6, 7, 8, 9, 10, 11, 13].into_iter().collect();
        for (i, meta) in built.accounts.iter().enumerate() {
            // Position 9 excepted: this fill's buyer spent from an AUX
            // base account, not the canonical ATA (same class as the
            // sim probes' aux-buyer resample rule). OUR builds always
            // use the canonical ATA — the idempotent prefix guarantees
            // it exists — so the builder is correct and the sample
            // simply isn't byte-equal there.
            if i != 9 {
                assert_eq!(meta.pubkey, key(i), "position {i}");
            }
            let expect = if i == 8 {
                AccountMeta::new(key(i), true)
            } else if writable.contains(&i) {
                AccountMeta::new(key(i), false)
            } else {
                AccountMeta::new_readonly(key(i), false)
            };
            if i != 9 {
                assert_eq!(*meta, expect, "flags {i}");
            } else {
                // Aux position: flags convention still asserted,
                // pubkey excluded (see above).
                assert_eq!(meta.is_signer, expect.is_signer, "flags {i}");
                assert_eq!(meta.is_writable, expect.is_writable, "flags {i}");
            }
        }
        assert_eq!(built.data, data);
    }

    #[test]
    fn extended_buy_matches_frozen_mayhem_sample() {
        // Byte-equality against a real mainnet mayhem buy (frozen tx):
        // our extended shape must reproduce positions 0..16 exactly.
        // Positions [16],[17] are per-launch mayhem routing accounts
        // outside this builder's verified map (see module docs).
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../listener/tests/fixtures/mayhem_buy_mainnet.json"
        ))
        .expect("fixture");
        let msg = &raw["transaction"]["message"];
        let keys: Vec<String> = serde_json::from_value(msg["accountKeys"].clone()).expect("keys");
        let pump = PUMP_PROGRAM_ID.to_string();
        let pi = keys.iter().position(|k| k == &pump).expect("pump key");
        let mut found = None;
        for ix in msg["instructions"].as_array().expect("ixs") {
            if ix["programIdIndex"].as_u64() != Some(pi as u64) {
                continue;
            }
            let data = bs58::decode(ix["data"].as_str().expect("data"))
                .into_vec()
                .expect("b58");
            if data.starts_with(&PUMP_BUY_DISCRIMINATOR) && data.len() == 25 {
                let accounts: Vec<usize> = serde_json::from_value(ix["accounts"].clone()).expect("accts");
                found = Some((accounts, data));
                break;
            }
        }
        let (idx, data) = found.expect("mayhem buy in fixture");
        assert_eq!(idx.len(), 18);
        let key = |i: usize| Pubkey::from_str(&keys[idx[i]]).expect("pubkey");
        // Creator hypothesis under test: the 32-byte chunk after the v2
        // (name,symbol,uri) strings. Vault match below proves it.
        let creator = Pubkey::new_from_array([
            181, 33, 156, 169, 71, 74, 115, 4, 142, 47, 129, 175, 116, 186, 124, 26, 181, 141,
            124, 30, 200, 103, 36, 11, 1, 87, 83, 217, 77, 172, 67, 38,
        ]);
        assert_eq!(creator_vault_pda(&creator), key(9));
        assert_eq!(user_volume_accumulator_pda(&key(6)), key(13));
        let amount = u64::from_le_bytes(data[8..16].try_into().expect("u64"));
        let max = u64::from_le_bytes(data[16..24].try_into().expect("u64"));
        // Frozen sample tracks volume (25-byte payload, trailing 0x01).
        assert_eq!(data[24..], [1u8]);
        let built = PumpSwapBuilder::build_swap_ix(
            SwapKind::Buy { amount, max_sol_cost: max, track_volume: true },
            &SwapAccounts {
                payer: &key(6),
                mint: &key(2),
                bonding_curve: &key(3),
                associated_bonding_curve: &key(4),
                user_ata: &key(5),
                fee_recipient: &key(1),
                token_program: &key(8),
                creator: Some(&creator),
                // No routing: locks the retired 16-account prefix
                // (positions 0..16 of the 18-account chain shape).
                // Dispatch never sends this standalone (6062); the
                // next test proves the dispatched 18-account form.
                mayhem_routing: None,
            },
        );
        assert_eq!(built.accounts.len(), 16);
        for (i, meta) in built.accounts.iter().enumerate() {
            assert_eq!(meta.pubkey, key(i), "position {i}");
        }
        assert_eq!(built.data, data);
    }

    #[test]
    fn mayhem_buy_appends_routing_to_18() {
        // Same frozen mayhem buy, full 18 accounts: with reference
        // routing the builder reproduces [16]/[17] exactly. Positions
        // 0..16 reuse the proof above; flags for the appended pair
        // follow vault convention (writable) — the sim gate, not this
        // test, proves the flags (the fixture carries no flag table).
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../listener/tests/fixtures/mayhem_buy_mainnet.json"
        ))
        .expect("fixture");
        let msg = &raw["transaction"]["message"];
        let keys: Vec<String> = serde_json::from_value(msg["accountKeys"].clone()).expect("keys");
        let pump = PUMP_PROGRAM_ID.to_string();
        let pi = keys.iter().position(|k| k == &pump).expect("pump key");
        let mut found = None;
        for ix in msg["instructions"].as_array().expect("ixs") {
            if ix["programIdIndex"].as_u64() != Some(pi as u64) {
                continue;
            }
            let data = bs58::decode(ix["data"].as_str().expect("data"))
                .into_vec()
                .expect("b58");
            if data.starts_with(&PUMP_BUY_DISCRIMINATOR) && data.len() == 25 {
                let accounts: Vec<usize> = serde_json::from_value(ix["accounts"].clone()).expect("accts");
                found = Some((accounts, data));
                break;
            }
        }
        let (idx, data) = found.expect("mayhem buy in fixture");
        assert_eq!(idx.len(), 18);
        let key = |i: usize| Pubkey::from_str(&keys[idx[i]]).expect("pubkey");
        let creator = Pubkey::new_from_array([
            181, 33, 156, 169, 71, 74, 115, 4, 142, 47, 129, 175, 116, 186, 124, 26, 181, 141,
            124, 30, 200, 103, 36, 11, 1, 87, 83, 217, 77, 172, 67, 38,
        ]);
        let amount = u64::from_le_bytes(data[8..16].try_into().expect("u64"));
        let max = u64::from_le_bytes(data[16..24].try_into().expect("u64"));
        let routing = MayhemRouting { unknown_16: &key(16), pfee_vault: &key(17) };
        let built = PumpSwapBuilder::build_swap_ix(
            SwapKind::Buy { amount, max_sol_cost: max, track_volume: true },
            &SwapAccounts {
                payer: &key(6),
                mint: &key(2),
                bonding_curve: &key(3),
                associated_bonding_curve: &key(4),
                user_ata: &key(5),
                fee_recipient: &key(1),
                token_program: &key(8),
                creator: Some(&creator),
                mayhem_routing: Some(routing),
            },
        );
        assert_eq!(built.accounts.len(), 18);
        for (i, meta) in built.accounts.iter().enumerate() {
            assert_eq!(meta.pubkey, key(i), "position {i}");
        }
        assert!(built.accounts[16].is_writable && !built.accounts[16].is_signer);
        assert!(built.accounts[17].is_writable && !built.accounts[17].is_signer);
        assert_eq!(built.data, data);
    }

    #[test]
    fn program_bound_ata_differs_for_token_2022() {
        // Regression from a live mayhem sim: the classic helper derives
        // a DIFFERENT (wrong) address for Token-2022 mints, and trading
        // through it fails inside the ATA program (IncorrectProgramId).
        let owner = Pubkey::new_from_array([7; 32]);
        let mint = Pubkey::new_from_array([8; 32]);
        let classic = user_ata(&owner, &mint);
        let modern = user_ata_with_program(&owner, &mint, &TOKEN_PROGRAM_ID);
        assert_eq!(classic, modern, "classic program must agree");
        let token_2022 = Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").expect("t22");
        let modern22 = user_ata_with_program(&owner, &mint, &token_2022);
        assert_ne!(modern22, classic, "2022 derivation must differ");
        // Sanity: distinct owners still get distinct addresses.
        let other = user_ata_with_program(&Pubkey::new_from_array([9; 32]), &mint, &token_2022);
        assert_ne!(other, modern22);
    }

    #[test]
    fn compute_budget_prefix_shapes() {
        // Default: empty (existing transactions unchanged).
        assert!(compute_budget_ixs(0, 0).is_empty());
        // Limit only / price only.
        let limit = compute_budget_ixs(200_000, 0);
        assert_eq!(limit.len(), 1);
        assert_eq!(limit[0].program_id, COMPUTE_BUDGET_PROGRAM_ID);
        assert_eq!(limit[0].data[0], 2);
        assert_eq!(
            u32::from_le_bytes(limit[0].data[1..5].try_into().expect("u32")),
            200_000
        );
        // Both: limit first, then price (runtime convention).
        let both = compute_budget_ixs(200_000, 1_000);
        assert_eq!(both.len(), 2);
        assert_eq!(both[0].data[0], 2);
        assert_eq!(both[1].data[0], 3);
        assert_eq!(
            u64::from_le_bytes(both[1].data[1..9].try_into().expect("u64")),
            1_000
        );
        assert!(both[0].accounts.is_empty() && both[1].accounts.is_empty());
    }

    #[test]
    fn fee_recipient_is_caller_supplied() {
        // The IDL takes feeRecipient as a parameter account: rotation is a
        // config change, exercised here end to end.
        let (payer, mint, curve, assoc, ata) = keys();
        let custom = Pubkey::new_from_array([9; 32]);
        let ix = PumpSwapBuilder::build_swap_ix(
            SwapKind::Buy { amount: 1, max_sol_cost: 1, track_volume: false },
            &SwapAccounts {
                payer: &payer,
                mint: &mint,
                bonding_curve: &curve,
                associated_bonding_curve: &assoc,
                user_ata: &ata,
                fee_recipient: &custom,
                token_program: &TOKEN_PROGRAM_ID,
                creator: None,
                mayhem_routing: None,
            },
        );
        assert_eq!(ix.accounts[1], AccountMeta::new(custom, false));
        assert_eq!(ix.accounts.len(), 12);
    }

    #[test]
    fn buy_layout_and_accounts() {
        let (payer, mint, curve, assoc, ata) = keys();
        let ix = PumpSwapBuilder::build_swap_ix(
            SwapKind::Buy { amount: 1_000, max_sol_cost: 500, track_volume: false },
            &SwapAccounts {
                payer: &payer,
                mint: &mint,
                bonding_curve: &curve,
                associated_bonding_curve: &assoc,
                user_ata: &ata,
                fee_recipient: &FEE_RECIPIENT,
                token_program: &TOKEN_PROGRAM_ID,
                creator: None,
                mayhem_routing: None,
            },
        );
        assert_eq!(ix.program_id, PUMP_PROGRAM_ID);
        assert_eq!(&ix.data[..8], PUMP_BUY_DISCRIMINATOR);
        // 8 disc + 8 amount + 8 max + 1 bool flag.
        assert_eq!(ix.data.len(), 25);
        assert_eq!(ix.data[24], 0, "default wrapper leaves volume untracked");
        let args = PumpBuyArgs::try_from_slice(&ix.data[8..]).expect("decode");
        assert_eq!(
            args,
            PumpBuyArgs {
                amount: 1_000,
                max_sol_cost: 500,
                track_volume: false,
            }
        );
        assert_eq!(ix.accounts.len(), 12);
        // Spot-check order and mutability per the skill.
        assert_eq!(
            ix.accounts[0],
            AccountMeta::new_readonly(GLOBAL_ACCOUNT, false)
        );
        assert_eq!(ix.accounts[1], AccountMeta::new(FEE_RECIPIENT, false));
        assert_eq!(ix.accounts[6], AccountMeta::new(payer, true));
        // Buy carries token-program(8) then rent(9) per the skill; a prior
        // revision had these swapped (rent at 8), which fails on-chain.
        assert_eq!(
            ix.accounts[8],
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false)
        );
        assert_eq!(
            ix.accounts[9],
            AccountMeta::new_readonly(RENT_SYSVAR_ID, false)
        );
        assert_eq!(
            ix.accounts[11],
            AccountMeta::new_readonly(PUMP_PROGRAM_ID, false)
        );
        assert!(ix.accounts[6].is_signer);
    }

    #[test]
    fn sell_layout_and_accounts() {
        let (payer, mint, curve, assoc, ata) = keys();
        let ix = PumpSwapBuilder::build_swap_ix(
            SwapKind::Sell { amount: 700, min_sol_output: 300 },
            &SwapAccounts {
                payer: &payer,
                mint: &mint,
                bonding_curve: &curve,
                associated_bonding_curve: &assoc,
                user_ata: &ata,
                fee_recipient: &FEE_RECIPIENT,
                token_program: &TOKEN_PROGRAM_ID,
                creator: None,
                mayhem_routing: None,
            },
        );
        assert_eq!(&ix.data[..8], PUMP_SELL_DISCRIMINATOR);
        let args = PumpSellArgs::try_from_slice(&ix.data[8..]).expect("decode");
        assert_eq!(
            args,
            PumpSellArgs {
                amount: 700,
                min_sol_output: 300
            }
        );
        assert_eq!(ix.accounts.len(), 12);
        // Sell carries the ATA program at index 8 and token at 9
        // (buy carries token at 8 and rent at 9).
        assert_eq!(
            ix.accounts[8],
            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false)
        );
        assert_eq!(
            ix.accounts[9],
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false)
        );
    }

    #[test]
    fn ata_matches_pda_derivation() {
        let payer = Pubkey::new_from_array([11; 32]);
        let mint = Pubkey::new_from_array([12; 32]);
        let ata = user_ata(&payer, &mint);
        let (expected, _bump) = Pubkey::find_program_address(
            &[
                &payer.to_bytes(),
                &TOKEN_PROGRAM_ID.to_bytes(),
                &mint.to_bytes(),
            ],
            &ASSOCIATED_TOKEN_PROGRAM_ID,
        );
        assert_eq!(ata, expected);
    }
}
