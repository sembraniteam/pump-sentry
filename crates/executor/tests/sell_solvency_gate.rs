//! Integration: the sell-side solvency gate at the public
//! dispatch boundary. Exits must never demand the buy-side 0.05 SOL
//! reserve (a drained wallet must still be able to leave a bag), but a
//! wallet that cannot cover tip + priority + fee keeps refusing.
//!
//! Mocked reads only; no bundle client is attached, so `sell_jito`
//! fails at `ExecutorError::NoJito` — which certifies the preflight
//! returned instructions rather than `InsufficientFunds`. Nothing is
//! dispatched, simulated on-chain, or broadcast.

use executor::instructions::{
    CURVE_CREATOR_END, CURVE_CREATOR_OFFSET, CURVE_QUOTE_MINT_END, CURVE_QUOTE_MINT_OFFSET,
    FEE_CONFIG_PROGRAM, PUMP_PROGRAM_ID, TOKEN_PROGRAM_ID, WSOL_MINT,
};
use executor::{ExecutorError, SellOrder, SwapExecutor, WalletGuard, WalletGuardError};
use ids::BONDING_CURVE_DISCRIMINATOR;
use solana_client::{
    nonblocking::rpc_client::RpcClient, rpc_client::RpcClientConfig, rpc_request::RpcRequest,
};
use solana_commitment_config::CommitmentConfig;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::mock_sender::{MockSender, MocksMap};
use std::sync::Arc;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MockCtx {
    slot: u64,
    api_version: &'static str,
}

#[derive(serde::Serialize)]
struct MockResp<T: serde::Serialize> {
    context: MockCtx,
    value: T,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MockUiAcct {
    data: (String, String),
    executable: bool,
    lamports: u64,
    owner: String,
    rent_epoch: u64,
    space: u64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MockTokAmt {
    amount: String,
    decimals: u8,
    ui_amount_string: String,
}

fn mock_ok<T: serde::Serialize>(value: T) -> serde_json::Value {
    serde_json::to_value(MockResp {
        context: MockCtx {
            slot: 1,
            api_version: "4.3.0",
        },
        value,
    })
    .expect("mock response serializes")
}

fn mock_ui_account(owner: &Pubkey, data: &[u8]) -> serde_json::Value {
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
    serde_json::to_value(MockResp {
        context: MockCtx {
            slot: 1,
            api_version: "4.3.0",
        },
        value: MockUiAcct {
            data: (BASE64_STANDARD.encode(data), "base64".to_string()),
            executable: false,
            lamports: 1_000_000,
            owner: owner.to_string(),
            rent_epoch: 0,
            space: data.len() as u64,
        },
    })
    .expect("mock account serializes")
}

/// Synthetic curve blob (own layout, pinned offsets): WSOL quote,
/// live reserves, not complete, no mayhem. Mirrors the in-crate
/// `mock_curve` shape without a quote-mint skip.
fn curve_blob(creator: &Pubkey) -> Vec<u8> {
    let mut data = vec![0u8; 166];
    data[..8].copy_from_slice(&BONDING_CURVE_DISCRIMINATOR);
    data[CURVE_CREATOR_OFFSET..CURVE_CREATOR_END].copy_from_slice(creator.as_ref());
    data[CURVE_QUOTE_MINT_OFFSET..CURVE_QUOTE_MINT_END].copy_from_slice(WSOL_MINT.as_ref());
    // Virtual token / virtual quote / real token reserves.
    data[8..16].copy_from_slice(&1_000_000_000_000_000u64.to_le_bytes());
    data[16..24].copy_from_slice(&30_000_000_000u64.to_le_bytes());
    data[24..32].copy_from_slice(&800_000_000_000_000u64.to_le_bytes());
    data
}

/// Mock queue for one full curve-sell preflight (WSOL quote, live
/// scope). Per-method queues pop front-first: balance; then mint
/// owner, curve state, global gate, fee schedule, mint supply, WSOL
/// quote-mint owner (`GetAccountInfo`, in call order); then the
/// source-token balance.
fn sell_mocks(balance: u64, held: &str) -> MocksMap {
    let creator = Pubkey::new_from_array([9; 32]);
    let fee_bin: &[u8] = include_bytes!("../../listener/tests/fixtures/fee_config_live.bin");
    let mut global = vec![0u8; 1046];
    global[1045] = 1;
    let mut mint_blob = vec![0u8; 64];
    mint_blob[36..44].copy_from_slice(&1_000_000_000_000_000u64.to_le_bytes());
    let mut mocks = MocksMap::default();
    mocks.insert(RpcRequest::GetBalance, mock_ok(balance));
    mocks.insert(
        RpcRequest::GetAccountInfo,
        mock_ui_account(&TOKEN_PROGRAM_ID, &[0u8; 8]),
    );
    mocks.insert(
        RpcRequest::GetAccountInfo,
        mock_ui_account(&PUMP_PROGRAM_ID, &curve_blob(&creator)),
    );
    // preflight_sell_v3 reads the WSOL quote-mint owner next, then
    // re-quotes (global gate, fee schedule, mint supply).
    mocks.insert(
        RpcRequest::GetAccountInfo,
        mock_ui_account(&TOKEN_PROGRAM_ID, &[0u8; 8]),
    );
    mocks.insert(
        RpcRequest::GetAccountInfo,
        mock_ui_account(&PUMP_PROGRAM_ID, &global),
    );
    mocks.insert(
        RpcRequest::GetAccountInfo,
        mock_ui_account(&FEE_CONFIG_PROGRAM, fee_bin),
    );
    mocks.insert(
        RpcRequest::GetAccountInfo,
        mock_ui_account(&TOKEN_PROGRAM_ID, &mint_blob),
    );
    mocks.insert(
        RpcRequest::GetTokenAccountBalance,
        mock_ok(MockTokAmt {
            amount: held.to_string(),
            decimals: 6,
            ui_amount_string: held.to_string(),
        }),
    );
    mocks
}

fn mock_executor(mocks: MocksMap) -> SwapExecutor {
    let rpc = Arc::new(RpcClient::new_sender(
        MockSender::new_with_mocks_map("sell-solvency-gate", mocks),
        RpcClientConfig::with_commitment(CommitmentConfig::confirmed()),
    ));
    let wallet = WalletGuard::from_base58(&bs58::encode(Keypair::new().to_bytes()).into_string())
        .expect("wallet");
    SwapExecutor::with_rpc(rpc, wallet)
}

fn sell_order() -> SellOrder {
    SellOrder {
        mint: Pubkey::new_from_array([10; 32]),
        bonding_curve: Pubkey::new_from_array([11; 32]),
        associated_bonding_curve: Pubkey::new_from_array([11; 32]),
        amount_tokens: 100_000_000_000,
        min_sol_output: 1,
        tip_lamports: 0,
        slippage_bps: 200,
        creator: Some(Pubkey::new_from_array([9; 32])),
    }
}

/// A 0.04 SOL wallet — below the 0.05 SOL buy reserve, above every
/// sell leg — must construct the curve sell and only then run out of
/// dispatch plumbing (`NoJito`). At HEAD the sell preflight demanded
/// 52_144_280 lamports and refused before instruction construction.
#[tokio::test]
async fn curve_sell_with_low_balance_reaches_instruction_construction() {
    let exec = mock_executor(sell_mocks(40_000_000, "100000000000"));
    let err = exec
        .sell_jito(&sell_order())
        .await
        .expect_err("no bundle client: fails after instruction construction");
    assert!(
        matches!(err, ExecutorError::NoJito),
        "preflight must reach instruction construction with a 0.04 SOL wallet, not refuse: {err}"
    );
}

/// A wallet that cannot cover tip + priority + fee + the rent-exempt
/// floor still refuses, with the sell-specific variant naming the
/// shortfall legs (never the 0.05-SOL buy-reserve framing).
#[tokio::test]
async fn unpayable_sell_refuses_before_instruction_construction() {
    let mut mocks = MocksMap::default();
    // 100_000 lamports < 5_000 fee + 890_880 rent-exempt floor.
    mocks.insert(RpcRequest::GetBalance, mock_ok(100_000u64));
    let exec = mock_executor(mocks);
    let err = exec
        .sell_jito(&sell_order())
        .await
        .expect_err("below every fee leg must refuse");
    assert!(
        matches!(
            err,
            ExecutorError::Wallet(WalletGuardError::SellInsufficientFunds { .. })
        ),
        "sell-specific refusal expected: {err}"
    );
}
