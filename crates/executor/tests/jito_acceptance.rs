//! Jito mainnet acceptance probe (Syarat A).
//!
//! Sends one dummy bundle through OUR code path (`encode_bundle` +
//! `bundle_payload` + [`JitoClient::send_bundle`]) to the real mainnet
//! block engine and asserts acceptance: a UUID `result` with no `-32602`
//! decode error. That proves the engine decodes our base64 payload with
//! the `{"encoding": "base64"}` param — the exact A6 regression.
//!
//! COST: $0 by construction. The payer is a freshly generated random
//! keypair holding no SOL, so even a landed bundle moves nothing. The tip
//! is exactly the engine minimum (1000 lamports) so the probe passes
//! economic validation; an empty payer cannot pay it anyway.
//!
//! NEVER runs in normal `cargo test` (`#[ignore]`) nor CI. Run explicitly:
//! `JITO_ACCEPTANCE=1 cargo test -p executor --test jito_acceptance -- --ignored --nocapture`

use executor::{
    JITO_TIP_ACCOUNTS, JitoClient, bundle_payload,
    jito::{encode_bundle, tip_instruction},
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;

mod common;

/// Live probe: dummy bundle acceptance on mainnet block engine.
#[tokio::test]
#[ignore]
async fn mainnet_bundle_acceptance() {
    if std::env::var("JITO_ACCEPTANCE").as_deref() != Ok("1") {
        eprintln!("skipped: set JITO_ACCEPTANCE=1 to run the live Jito probe");
        return;
    }

    // 1. Fresh unfunded payer: nothing of value can move, by construction.
    let payer = Keypair::new();
    let payer_pubkey = payer.pubkey();
    let tip = Pubkey::from_str_const(JITO_TIP_ACCOUNTS[0]);

    // 2. One minimum-tip transfer (valid instruction; the engine
    //    requires >= 1000 lamports or it refuses the bundle outright).
    //    The payer is empty, so nothing can move regardless.
    let ix = tip_instruction(&payer_pubkey, &tip, 1_000);
    assert_eq!(ix.data.len(), 12);

    // 3. Fresh blockhash (free read-only RPC) + local signing (free).
    let rpc = common::rpc_client();
    let blockhash = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        rpc.get_latest_blockhash(),
    )
    .await
    .expect("blockhash fetch timed out")
    .expect("blockhash fetch failed");
    let tx = Transaction::new_signed_with_payer(
        std::slice::from_ref(&ix),
        Some(&payer_pubkey),
        &[&payer],
        blockhash,
    );

    // 4. OUR exact wire path: encode -> payload (assert the A6 param!).
    let bundle = encode_bundle(std::slice::from_ref(&tx)).expect("encode");
    let payload = bundle_payload(bundle.clone());
    assert_eq!(payload["method"], "sendBundle");
    assert_eq!(payload["params"][1]["encoding"], "base64");

    // 5. Fire at the real engine; acceptance = UUID result.
    let jito = JitoClient::new("https://mainnet.block-engine.jito.wtf")
        .expect("engine URL must parse");
    let uuid = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        jito.send_bundle(bundle),
    )
    .await
    .expect("engine call timed out")
    .expect("engine rejected the bundle (see error: -32602 = decode failure)");
    assert!(!uuid.is_empty(), "empty bundle id");
    println!("ACCEPTED bundle_id={uuid}");
}
