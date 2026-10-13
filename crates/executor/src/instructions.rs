//! Pump.fun buy/sell instruction builders (skill `solana-pumpfun-swap`).
//!
//! Layouts and account order follow the skill exactly. Program addresses
//! are compile-time constants; SPL program IDs cross-check against the
//! `spl-token` / `spl-associated-token-account` crates so version drift
//! fails loudly in tests instead of on-chain.
//!
//! SHAPES BUILT HERE (v3 only):
//! - 17-account v3 `buy_v3`/`sell_v3` shapes
//!   ([`PumpSwapBuilder::build_buy_v3_ix`]/[`PumpSwapBuilder::build_sell_v3_ix`]):
//!   the official TRADE_V3.md layouts — fees stay on the curve, so no
//!   fee recipient, creator vault, or sharing config travels. Both sides
//!   byte-exact vs frozen mainnet samples (positions + payload +
//!   re-derived PDA/ATAs, one shared 17-account body).
//! - 17-account `buy_exact_quote_in_v3` shape
//!   ([`PumpSwapBuilder::build_buy_exact_quote_in_v3_ix`]): same body,
//!   exact-spend payload.
//!
//! RETIRED SHAPES (deleted, not deprecated): the 12-account legacy
//! `buy`/`sell` core, the 16/18-account mayhem buy, and the 27/26-account
//! `buy_v2`/`sell_v2` shapes. Mainnet evidence as of Oct-2026: legacy
//! fills are extinct (probes SKIP), and 18/v2 fills occur only on old
//! curves — every new launch is a v3 curve, so same-curve reference
//! routing could never resolve for sniper entries anyway. Preflight
//! refuses non-v3 curves fail-closed (loud diverged/manual, no funds
//! move); the listener still parses all historical shapes for
//! screening and quorum.
//!
//! RUNTIME (not build-time) ROUTING — never hardcoded here:
//! - v3 needs no fee recipient (fees stay on the curve); only the
//!   buyback pick travels (any of the 8 published recipients).
//! - Token program resolves per mint (classic SPL vs Token-2022); the
//!   ATA derives bound to it (a 2022 mint through the classic program
//!   fails with `IncorrectProgramId`, verified live).

use borsh::{BorshDeserialize, BorshSerialize};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use std::str::FromStr;

pub use ids::{
    ASSOCIATED_TOKEN_PROGRAM_ID, BUYBACK_FEE_RECIPIENTS,
    CURVE_CASHBACK_FLAG_OFFSET, CURVE_COMPLETE_OFFSET, CURVE_CREATOR_END, CURVE_CREATOR_OFFSET,
    CURVE_MAYHEM_FLAG_OFFSET, CURVE_MIN_LEN, CURVE_QUOTE_MINT_END, CURVE_QUOTE_MINT_OFFSET,
    DEFAULT_BUYBACK_FEE_RECIPIENT, EVENT_AUTHORITY, FEE_CONFIG_ACCOUNT, FEE_CONFIG_PROGRAM,
    FEE_RECIPIENT, GLOBAL_ACCOUNT, PUMP_BUY_DISCRIMINATOR,
    PUMP_BUY_V2_DISCRIMINATOR, PUMP_BUY_V3_DISCRIMINATOR, PUMP_BUY_EXACT_QUOTE_IN_V3_DISCRIMINATOR,
    PUMP_CREATE_V2_DISCRIMINATOR, PUMP_PROGRAM_ID, PUMP_SELL_V3_DISCRIMINATOR,
    RENT_SYSVAR_ID, SYSTEM_PROGRAM_ID,
    TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, USDC_MINT, USER_VOLUME_ACCUMULATOR_SEED,
    WSOL_MINT,
};

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
/// 24 bytes (discriminator + two u64s).
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

/// Stateless builder for Pump.fun swap instructions (v3 only — see
/// the module docs for the retired-shapes rationale).
pub struct PumpSwapBuilder;

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

/// User volume-accumulator PDA: `["user_volume_accumulator", user]`.
/// Seed and derivation verified byte-exact against live v3 fills; the
/// PDA travels at v3 position [11] on every dispatch.
pub fn user_volume_accumulator_pda(user: &Pubkey) -> Pubkey {
    let (pda, _) = Pubkey::find_program_address(
        &[USER_VOLUME_ACCUMULATOR_SEED, user.as_ref()],
        &PUMP_PROGRAM_ID,
    );
    pda
}

impl PumpSwapBuilder {
    /// v3 buy instruction: the 17-account shape for entries on any
    /// non-cashback curve. Fees stay on the curve, so neither a fee
    /// recipient nor a same-curve reference is needed — only the
    /// buyback pick plus derived addresses. Writability byte-proven
    /// against frozen mainnet samples; preflight simulation gates
    /// every build (the 17-shape sim probe is the gate).
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
