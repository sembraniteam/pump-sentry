//! Pump.fun buy/sell builders (v3 only).
//!
//! Layouts and account order are byte-exact vs frozen mainnet samples.
//! SPL IDs cross-check the SPL crates, so drift fails in tests.
//!
//! Shapes: 17-account `buy_v3`/`sell_v3` plus 17-account
//! `buy_exact_quote_in_v3`. Legacy/18/v2 shapes are deleted;
//! non-v3 curves fail closed in preflight.
//!
//! Routing is runtime: v3 needs only the buyback pick (any of the 8);
//! the token program resolves per mint, and the ATA binds to it.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use std::str::FromStr;

pub use ids::{
    ASSOCIATED_TOKEN_PROGRAM_ID, BUYBACK_FEE_RECIPIENTS, COMPUTE_BUDGET_PROGRAM_ID,
    CURVE_CASHBACK_FLAG_OFFSET, CURVE_COIN_CREATOR_FEE_END, CURVE_COMPLETE_OFFSET,
    CURVE_CREATOR_END, CURVE_CREATOR_OFFSET,
    CURVE_MAYHEM_FLAG_OFFSET, CURVE_QUOTE_MINT_END, CURVE_QUOTE_MINT_OFFSET,
    DEFAULT_BUYBACK_FEE_RECIPIENT, EVENT_AUTHORITY, FEE_CONFIG_ACCOUNT, FEE_CONFIG_PROGRAM,
    FEE_RECIPIENT, GLOBAL_ACCOUNT, PUMP_AMM_EVENT_AUTHORITY_SEED, PUMP_AMM_FEE_CONFIG,
    PUMP_AMM_GLOBAL_CONFIG, PUMP_AMM_PROGRAM_ID, PUMP_AMM_SELL_V2_DISCRIMINATOR,
    PUMP_BUY_V3_DISCRIMINATOR, PUMP_BUY_EXACT_QUOTE_IN_V3_DISCRIMINATOR, PUMP_PROGRAM_ID,
    PUMP_SELL_V3_DISCRIMINATOR, RENT_SYSVAR_ID, SYSTEM_PROGRAM_ID, TOKEN_PROGRAM_ID,
    TOKEN_2022_PROGRAM_ID, USDC_MINT, USER_VOLUME_ACCUMULATOR_SEED, WSOL_MINT,
};

/// v3 buy args. 24-byte payload with no `partial_fill` tail.
#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq)]
pub struct PumpBuyV3Args {
    /// Token base units requested.
    pub amount: u64,
    /// Max quote allowed (fees in).
    pub max_sol_cost: u64,
}

/// v3 sell args: 24-byte payload.
#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq)]
pub struct PumpSellV3Args {
    /// Token base units to sell.
    pub amount: u64,
    /// Min quote to receive.
    pub min_sol_output: u64,
}

/// v3 exact-quote buy args: same 17 accounts, 24-byte payload.
#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq)]
pub struct PumpBuyExactQuoteInV3Args {
    /// Exact quote to spend, fees in.
    pub spendable_quote_in: u64,
    /// Fewest base tokens accepted.
    pub min_tokens_out: u64,
}

/// Stateless v3 builder.
pub struct PumpSwapBuilder;

/// v3 buy accounts (17). No fee recipient: fees stay on the curve.
/// Buyback is any of the 8 published (its quote ATA on token quotes).
#[derive(Debug, Clone, Copy)]
pub struct BuyV3Accounts<'a> {
    /// Payer and signer (position 8).
    pub payer: &'a Pubkey,
    /// Token mint (position 1).
    pub mint: &'a Pubkey,
    /// Quote mint (position 2; WSOL on SOL pairs).
    pub quote_mint: &'a Pubkey,
    /// Curve PDA (position 5).
    pub bonding_curve: &'a Pubkey,
    /// Curve base ATA (position 6).
    pub associated_bonding_curve: &'a Pubkey,
    /// Token program owning `mint`.
    pub base_token_program: &'a Pubkey,
    /// Token program owning `quote_mint`.
    pub quote_token_program: &'a Pubkey,
    /// Buyback recipient (position 13).
    pub buyback_recipient: &'a Pubkey,
}

/// v3 sell accounts (17). Same shape, drain direction.
#[derive(Debug, Clone, Copy)]
pub struct SellV3Accounts<'a> {
    /// Payer and signer (position 8).
    pub payer: &'a Pubkey,
    /// Token mint (position 1).
    pub mint: &'a Pubkey,
    /// Quote mint (position 2; WSOL on SOL pairs).
    pub quote_mint: &'a Pubkey,
    /// Curve PDA (position 5).
    pub bonding_curve: &'a Pubkey,
    /// Curve base ATA (position 6).
    pub associated_bonding_curve: &'a Pubkey,
    /// Token program owning `mint`.
    pub base_token_program: &'a Pubkey,
    /// Token program owning `quote_mint`.
    pub quote_token_program: &'a Pubkey,
    /// Buyback recipient (position 13).
    pub buyback_recipient: &'a Pubkey,
}

/// User volume PDA: `["user_volume_accumulator", user]` (v3 [11]).
pub fn user_volume_accumulator_pda(user: &Pubkey) -> Pubkey {
    let (pda, _) = Pubkey::find_program_address(
        &[USER_VOLUME_ACCUMULATOR_SEED, user.as_ref()],
        &PUMP_PROGRAM_ID,
    );
    pda
}

impl PumpSwapBuilder {
    /// v3 buy ix. Sim gates every build.
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

    /// v3 sell ix: same 17 accounts, drain direction.
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

    /// Shared 17-account v3 body: `data` picks the side.
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

    /// v3 exact-quote buy: spend `spendable_quote_in` for `min_tokens_out`.
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

/// PumpSwap `sell_v2` args: 24-byte payload.
#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq)]
pub struct PumpAmmSellV2Args {
    /// Exact base tokens to sell.
    pub base_amount_in: u64,
    /// Least quote accepted after all fees.
    pub min_quote_amount_out: u64,
}

/// PumpSwap `sell_v2` accounts (17, doc order). Fees stay in the pool.
#[derive(Debug, Clone, Copy)]
pub struct PumpAmmSellV2Accounts<'a> {
    /// Pool account.
    pub pool: &'a Pubkey,
    /// Payer and signer.
    pub user: &'a Pubkey,
    /// Base mint (sold token).
    pub base_mint: &'a Pubkey,
    /// Quote mint (WSOL on SOL pools).
    pub quote_mint: &'a Pubkey,
    /// User base token account (must exist).
    pub user_base_token_account: &'a Pubkey,
    /// User quote token account (must exist).
    pub user_quote_token_account: &'a Pubkey,
    /// Pool base vault.
    pub pool_base_token_account: &'a Pubkey,
    /// Pool quote vault.
    pub pool_quote_token_account: &'a Pubkey,
    /// Token program owning `base_mint`.
    pub base_token_program: &'a Pubkey,
    /// Token program owning `quote_mint`.
    pub quote_token_program: &'a Pubkey,
    /// Buyback recipient quote ATA (must exist, never created).
    pub buyback_fee_recipient: &'a Pubkey,
}

/// PumpSwap user-volume PDA (program-created when missing).
pub fn amm_user_volume_accumulator_pda(user: &Pubkey) -> Pubkey {
    let (pda, _) = Pubkey::find_program_address(
        &[USER_VOLUME_ACCUMULATOR_SEED, user.as_ref()],
        &PUMP_AMM_PROGRAM_ID,
    );
    pda
}

/// PumpSwap event-authority PDA.
pub fn amm_event_authority_pda() -> Pubkey {
    let (pda, _) = Pubkey::find_program_address(
        &[PUMP_AMM_EVENT_AUTHORITY_SEED],
        &PUMP_AMM_PROGRAM_ID,
    );
    pda
}

impl PumpSwapBuilder {
    /// PumpSwap `sell_v2` ix: 17 accounts per `PUMP_SWAP_TRADE_V2.md`.
    /// Sim gates every build.
    pub fn build_sell_pumpswap_v2_ix(
        base_amount_in: u64,
        min_quote_amount_out: u64,
        accts: &PumpAmmSellV2Accounts<'_>,
    ) -> Instruction {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&PUMP_AMM_SELL_V2_DISCRIMINATOR);
        PumpAmmSellV2Args { base_amount_in, min_quote_amount_out }
            .serialize(&mut data)
            .expect("in-memory Borsh encode cannot fail");
        let accounts = vec![
            AccountMeta::new(*accts.pool, false),                          // [0]
            AccountMeta::new(*accts.user, true),                           // [1]
            AccountMeta::new_readonly(PUMP_AMM_GLOBAL_CONFIG, false),      // [2]
            AccountMeta::new_readonly(*accts.base_mint, false),            // [3]
            AccountMeta::new_readonly(*accts.quote_mint, false),           // [4]
            AccountMeta::new(*accts.user_base_token_account, false),       // [5]
            AccountMeta::new(*accts.user_quote_token_account, false),      // [6]
            AccountMeta::new(*accts.pool_base_token_account, false),       // [7]
            AccountMeta::new(*accts.pool_quote_token_account, false),      // [8]
            AccountMeta::new_readonly(*accts.base_token_program, false),   // [9]
            AccountMeta::new_readonly(*accts.quote_token_program, false),  // [10]
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),           // [11]
            AccountMeta::new(amm_user_volume_accumulator_pda(accts.user), false), // [12]
            AccountMeta::new_readonly(PUMP_AMM_FEE_CONFIG, false),         // [13]
            AccountMeta::new(*accts.buyback_fee_recipient, false),         // [14]
            AccountMeta::new_readonly(amm_event_authority_pda(), false),   // [15]
            AccountMeta::new_readonly(PUMP_AMM_PROGRAM_ID, false),         // [16]
        ];
        debug_assert_eq!(accounts.len(), 17);
        Instruction { program_id: PUMP_AMM_PROGRAM_ID, accounts, data }
    }
}

/// ATA bound to an explicit token program. Needed for Token-2022:
/// seeds are `[owner, program, mint]`, so a program-less helper would
/// be wrong there (and every production caller passes the program).
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

/// Compute-budget prefix: limit then price. Zeros skipped.
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
/// Idempotent create-ATA ix. Safe on existing ATAs and check-vs-land races.
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
            // Per-mint program: 2022 via classic fails (`IncorrectProgramId`).
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new_readonly(RENT_SYSVAR_ID, false),
        ],
        // Idempotent (`1`, not `0`).
        data: vec![1],
    }
}

/// SPL address cross-check (drift detector). Test-only: no production
/// caller gates on it.
#[cfg(test)]
pub fn spl_ids_match() -> bool {
    TOKEN_PROGRAM_ID.to_bytes() == spl_token::ID.to_bytes()
        && ASSOCIATED_TOKEN_PROGRAM_ID.to_bytes() == spl_associated_token_account::ID.to_bytes()
}

/// Parse helper for plumbing and tests.
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
        // Byte-equality vs mainnet `sell_v3` fill (17 accounts).
        use std::collections::HashSet;
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../listener/tests/fixtures/sell_v3_mainnet.json"
        ))
        .expect("fixture");
        // Shape: bare getTransaction result.
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
    fn sell_pumpswap_v2_matches_documented_layout() {
        // Layout cross-checked against live sell_v2 frames by the
        // `pumpswap_sell_v2_simulates` acceptance probe (globals, fee
        // config, program slot, both PDA legs); this unit test pins the
        // 17-account table from PUMP_SWAP_TRADE_V2.md plus IDL
        // writability (pool/user/volumes writable, configs/programs
        // readonly, user signer). Still no frozen fill fixture.
        use std::collections::HashSet;
        let key = |b: u8| Pubkey::new_from_array([b; 32]);
        let accts = PumpAmmSellV2Accounts {
            pool: &PUMP_AMM_PROGRAM_ID,
            user: &key(1),
            base_mint: &key(3),
            quote_mint: &WSOL_MINT,
            user_base_token_account: &key(5),
            user_quote_token_account: &key(6),
            pool_base_token_account: &key(7),
            pool_quote_token_account: &key(8),
            base_token_program: &TOKEN_PROGRAM_ID,
            quote_token_program: &TOKEN_PROGRAM_ID,
            buyback_fee_recipient: &key(14),
        };
        let built = PumpSwapBuilder::build_sell_pumpswap_v2_ix(1_000_000, 900_000, &accts);
        assert_eq!(built.program_id, PUMP_AMM_PROGRAM_ID);
        assert_eq!(built.accounts.len(), 17);
        assert_eq!(&built.data[..8], &PUMP_AMM_SELL_V2_DISCRIMINATOR);
        assert_eq!(
            u64::from_le_bytes(built.data[8..16].try_into().expect("u64")),
            1_000_000
        );
        assert_eq!(
            u64::from_le_bytes(built.data[16..24].try_into().expect("u64")),
            900_000
        );
        assert_eq!(built.data.len(), 24);
        let writable: HashSet<usize> =
            [0, 1, 5, 6, 7, 8, 12, 14].into_iter().collect();
        let expect_keys = [
            PUMP_AMM_PROGRAM_ID, key(1), PUMP_AMM_GLOBAL_CONFIG, key(3), WSOL_MINT, key(5),
            key(6), key(7), key(8), TOKEN_PROGRAM_ID, TOKEN_PROGRAM_ID, SYSTEM_PROGRAM_ID,
            amm_user_volume_accumulator_pda(&key(1)), PUMP_AMM_FEE_CONFIG, key(14),
            amm_event_authority_pda(), PUMP_AMM_PROGRAM_ID,
        ];
        for (i, meta) in built.accounts.iter().enumerate() {
            assert_eq!(meta.pubkey, expect_keys[i], "position {i}");
            let expect = if i == 1 {
                AccountMeta::new(expect_keys[i], true)
            } else if writable.contains(&i) {
                AccountMeta::new(expect_keys[i], false)
            } else {
                AccountMeta::new_readonly(expect_keys[i], false)
            };
            assert_eq!(*meta, expect, "flags {i}");
        }
    }

    #[test]
    fn buy_v3_matches_frozen_sample() {
        // Byte-equality vs mainnet `buy_v3` fill (17 accounts, no tail).
        use std::collections::HashSet;
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../listener/tests/fixtures/buy_v3_mainnet.json"
        ))
        .expect("fixture");
        // Shape: bare getTransaction result.
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
        // Byte-equality vs mainnet `buy_exact_quote_in_v3` fill (17 accounts).
        use std::collections::HashSet;
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../listener/tests/fixtures/buy_exact_quote_in_v3_mainnet.json"
        ))
        .expect("fixture");
        // Shape: bare getTransaction result.
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
            // Position 9 excepted: this fill spent from an AUX base account,
            // not the canonical ATA. Our builds always use the canonical ATA.
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
                // Aux position: flags still asserted, pubkey excluded.
                assert_eq!(meta.is_signer, expect.is_signer, "flags {i}");
                assert_eq!(meta.is_writable, expect.is_writable, "flags {i}");
            }
        }
        assert_eq!(built.data, data);
    }

    #[test]
    fn program_bound_ata_differs_for_token_2022() {
        // Classic derivation is wrong for 2022 mints (`IncorrectProgramId`).
        let owner = Pubkey::new_from_array([7; 32]);
        let mint = Pubkey::new_from_array([8; 32]);
        let classic = user_ata_with_program(&owner, &mint, &TOKEN_PROGRAM_ID);
        let token_2022 = Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").expect("t22");
        let modern22 = user_ata_with_program(&owner, &mint, &token_2022);
        assert_ne!(modern22, classic, "2022 derivation must differ");
        // Sanity: distinct owners still get distinct addresses.
        let other = user_ata_with_program(&Pubkey::new_from_array([9; 32]), &mint, &token_2022);
        assert_ne!(other, modern22);
    }

    #[test]
    fn compute_budget_prefix_shapes() {
        assert!(compute_budget_ixs(0, 0).is_empty());
        // Limit only / price only; both: limit first, then price.
        let limit = compute_budget_ixs(200_000, 0);
        assert_eq!(limit.len(), 1);
        assert_eq!(limit[0].program_id, COMPUTE_BUDGET_PROGRAM_ID);
        assert_eq!(limit[0].data[0], 2);
        assert_eq!(
            u32::from_le_bytes(limit[0].data[1..5].try_into().expect("u32")),
            200_000
        );
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
        let ata = user_ata_with_program(&payer, &mint, &TOKEN_PROGRAM_ID);
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
