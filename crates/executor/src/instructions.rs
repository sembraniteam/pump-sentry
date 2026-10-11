//! Pump.fun buy/sell instruction builders (skill `solana-pumpfun-swap`).
//!
//! Layouts and account order follow the skill exactly. Program addresses
//! are compile-time constants; SPL program IDs cross-check against the
//! `spl-token` / `spl-associated-token-account` crates so version drift
//! fails loudly in tests instead of on-chain.
//!
//! ASSUMPTIONS REQUIRING MAINNET PROOF BEFORE FUNDING:
//! - 12-account legacy `buy`/`sell` layout. Newer flows (`buy_v2` with
//!   volume tracking, extra vault/fee accounts) are NOT built here; if
//!   Pump.fun routes new coins through them, these instructions fail.
//! - Single hardcoded `FEE_RECIPIENT`. If the program rotates recipients
//!   per transaction (8-way selection from Global is documented), the
//!   recipient must be read on-chain at runtime instead.
//!
//! Verify both against one recent mainnet buy/sell (see README TODO).

use borsh::{BorshDeserialize, BorshSerialize};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use std::str::FromStr;

pub use ids::{
    ASSOCIATED_TOKEN_PROGRAM_ID, CREATOR_VAULT_SEED, EVENT_AUTHORITY, FEE_CONFIG_ACCOUNT,
    FEE_CONFIG_PROGRAM, FEE_RECIPIENT, GLOBAL_ACCOUNT, GLOBAL_VOLUME_ACCUMULATOR,
    PUMP_BUY_DISCRIMINATOR, PUMP_BUY_V2_DISCRIMINATOR, PUMP_PROGRAM_ID, PUMP_SELL_DISCRIMINATOR,
    RENT_SYSVAR_ID, SYSTEM_PROGRAM_ID, TOKEN_PROGRAM_ID, USER_VOLUME_ACCUMULATOR_SEED,
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

/// Stateless builder for Pump.fun swap instructions.
///
/// LIVE SHAPE STATUS (verified 2026-10-11 against mainnet; see the
/// ignored layout-acceptance tests, the gate that must turn green):
/// the program's current mayhem buy is 18 accounts. Positions 0–15 are
/// FULLY MAPPED and byte-proven against frozen samples (legacy core +
/// live token program + Global recipient + creator vault + volume/fee
/// routing). Positions [16],[17] are per-token buyback-routing accounts
/// whose derivation is still unknown: [17] is a pfee-owned vault
/// (receives SOL in successful buys), [16] does not exist pre-first-buy.
/// Omitting them fails simulation with `BuybackFeeRecipientMissing`
/// (6062) — the gate stays red until they resolve. ~100 PDA seed
/// combinations across pump/mayhem/pfee programs found nothing.
/// Candidate next step: copy [16],[17] from a same-token reference buy
/// at trade time (proven per-token stable across transactions).
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
    /// Fee recipient (resolved live: override, Global, then constant).
    pub fee_recipient: &'a Pubkey,
    /// Token program owning `mint` (classic SPL vs Token-2022).
    pub token_program: &'a Pubkey,
    /// Token creator: enables the 16-account extended shape (creator
    /// vault at 9 plus volume/fee-config routing at 12–15), verified
    /// against live mayhem buys. `None` keeps the legacy 12-account
    /// core (classic curves; rejected on mayhem curves).
    pub creator: Option<&'a Pubkey>,
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

        Instruction { program_id: PUMP_PROGRAM_ID, accounts, data }
    }

    /// Buy instruction: spends SOL for `amount` tokens, capped at
    /// `max_sol_cost` lamports.
    ///
    /// Deprecated: bakes in the canonical fee recipient, classic token
    /// program, no creator, and untracked volume. Prefer
    /// [`PumpSwapBuilder::build_swap_ix`] with an explicit
    /// [`SwapAccounts`] (live recipient + per-mint program + creator).
    #[deprecated(
        since = "0.1.0",
        note = "use build_swap_ix with explicit SwapAccounts; this wrapper cannot express mayhem routing"
    )]
    #[allow(clippy::too_many_arguments)]
    pub fn build_buy_ix(
        payer: &Pubkey,
        mint: &Pubkey,
        bonding_curve: &Pubkey,
        associated_bonding_curve: &Pubkey,
        user_ata: &Pubkey,
        amount: u64,
        max_sol_cost: u64,
    ) -> Instruction {
        Self::build_swap_ix(
            SwapKind::Buy { amount, max_sol_cost, track_volume: false },
            &SwapAccounts {
                payer,
                mint,
                bonding_curve,
                associated_bonding_curve,
                user_ata,
                fee_recipient: &FEE_RECIPIENT,
                token_program: &TOKEN_PROGRAM_ID,
                creator: None,
            },
        )
    }

    /// Sell instruction: sells `amount` tokens for at least
    /// `min_sol_output` lamports.
    ///
    /// Deprecated: same legacy defaults as [`PumpSwapBuilder::build_buy_ix`];
    /// prefer [`PumpSwapBuilder::build_swap_ix`].
    #[deprecated(
        since = "0.1.0",
        note = "use build_swap_ix with explicit SwapAccounts; this wrapper cannot express mayhem routing"
    )]
    #[allow(clippy::too_many_arguments)]
    pub fn build_sell_ix(
        payer: &Pubkey,
        mint: &Pubkey,
        bonding_curve: &Pubkey,
        associated_bonding_curve: &Pubkey,
        user_ata: &Pubkey,
        amount: u64,
        min_sol_output: u64,
    ) -> Instruction {
        Self::build_swap_ix(
            SwapKind::Sell { amount, min_sol_output },
            &SwapAccounts {
                payer,
                mint,
                bonding_curve,
                associated_bonding_curve,
                user_ata,
                fee_recipient: &FEE_RECIPIENT,
                token_program: &TOKEN_PROGRAM_ID,
                creator: None,
            },
        )
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

fn pubkey_into_theirs(key: &Pubkey) -> solana_pubkey::Pubkey {
    solana_pubkey::Pubkey::new_from_array(key.to_bytes())
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
            },
        );
        assert_eq!(built.accounts.len(), 16);
        for (i, meta) in built.accounts.iter().enumerate() {
            assert_eq!(meta.pubkey, key(i), "position {i}");
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
            },
        );
        assert_eq!(ix.accounts[1], AccountMeta::new(custom, false));
        assert_eq!(ix.accounts.len(), 12);
    }

    #[test]
    #[allow(deprecated)]
    fn buy_layout_and_accounts() {
        let (payer, mint, curve, assoc, ata) = keys();
        let ix = PumpSwapBuilder::build_buy_ix(&payer, &mint, &curve, &assoc, &ata, 1_000, 500);
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
    #[allow(deprecated)]
    fn sell_layout_and_accounts() {
        let (payer, mint, curve, assoc, ata) = keys();
        let ix = PumpSwapBuilder::build_sell_ix(&payer, &mint, &curve, &assoc, &ata, 700, 300);
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
