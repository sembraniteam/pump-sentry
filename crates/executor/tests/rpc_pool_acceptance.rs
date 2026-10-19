//! RpcPool mainnet acceptance probe.
//!
//! Exercises the read-path failover pool against the REAL mainnet RPC:
//! dead-primary failover, sampler health/latency reporting, and a plain
//! balance read — the exact paths the bot's gate, preflights, and
//! custody recheck ride.
//!
//! COST: $0 by construction. Pure reads (`getSlot`, `getBalance`),
//! no wallet, no signing, nothing sent. NEVER runs in normal
//! `cargo test` (`#[ignore]`) nor CI. Run explicitly:
//! `RPC_POOL_ACCEPTANCE=1 cargo test -p executor --test rpc_pool_acceptance -- --ignored --nocapture`
//!
//! RPC: `MAINNET_RPC_URL` when set, else the public endpoint (slower,
//! rate-limited — rerun on throttled samples, don't "fix").

use executor::rpc_pool::{POOL_PROBE_SECS, RpcPool};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;

fn real_rpc_url() -> String {
    std::env::var("MAINNET_RPC_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".to_string())
}

/// Dead primary + real fallback: reads must ride the fallback and the
/// pool must say so (failover counter + latency on the live endpoint).
#[tokio::test]
#[ignore]
async fn dead_primary_fails_over_to_real_rpc() {
    if std::env::var("RPC_POOL_ACCEPTANCE").as_deref() != Ok("1") {
        eprintln!("skipped: set RPC_POOL_ACCEPTANCE=1 to run the live pool probe");
        return;
    }
    let real = real_rpc_url();
    let pool = RpcPool::build(vec!["http://127.0.0.1:9/".to_string(), real]);
    let client = pool.client(CommitmentConfig::confirmed());

    let slot = client
        .get_slot()
        .await
        .expect("failover serves getSlot on the real endpoint");
    assert!(slot > 400_000_000, "sanity: this is mainnet, slot={slot}");

    // A read-only balance (system program: no wallet, nothing owned).
    let system: Pubkey = "11111111111111111111111111111111".parse().unwrap();
    let lamports = client
        .get_balance(&system)
        .await
        .expect("failover serves getBalance");
    assert!(lamports > 0, "system program holds rent");

    let snap = pool.snapshot();
    eprintln!(
        "pool: endpoints={} healthy={} failover_reads={} latencies_us={:?} slot={slot}",
        snap.endpoints, snap.healthy, snap.failover_reads, snap.latencies_us
    );
    assert!(snap.failover_reads >= 1, "dead primary forced failover");
    assert!(snap.healthy >= 1, "real fallback stays eligible");
    assert!(
        snap.latencies_us[1] > 0,
        "live endpoint latency measured (index 1 = real RPC)"
    );
}

/// Cross-cluster divergence on real wire data: mainnet + devnet report
/// wildly different slots, so the sampler must flag inconsistency (and
/// the lagging peer stale) — the detector firing on real heterogeneous
/// data, not scripted values.
///
/// NOTE: a devnet URL as "fallback" is an operator misconfiguration in
/// production; here it is the instrument that proves the detector sees
/// real divergence. The pool is cluster-blind by design: it flags the
/// disagreement (so the heartbeat/operator sees it) while the eligible
/// freshest peer keeps serving.
#[tokio::test]
#[ignore]
async fn cross_cluster_divergence_flags_inconsistent() {
    if std::env::var("RPC_POOL_ACCEPTANCE").as_deref() != Ok("1") {
        eprintln!("skipped: set RPC_POOL_ACCEPTANCE=1 to run the live pool probe");
        return;
    }
    let pool = RpcPool::build(vec![
        "https://api.mainnet-beta.solana.com".to_string(),
        "https://api.devnet.solana.com".to_string(),
    ]);
    pool.spawn_sampler();
    // Two rounds: a single flaked probe must not decide the verdict.
    tokio::time::sleep(std::time::Duration::from_secs(2 * POOL_PROBE_SECS + 3)).await;

    let snap = pool.snapshot();
    eprintln!(
        "divergence: endpoints={} healthy={} primary_stale={} inconsistent={} failovers={} latencies_us={:?}",
        snap.endpoints,
        snap.healthy,
        snap.primary_stale,
        snap.inconsistent,
        snap.failover_reads,
        snap.latencies_us
    );
    assert_eq!(snap.endpoints, 2);
    assert!(
        snap.inconsistent,
        "mainnet/devnet slots differ by millions: views must flag inconsistent"
    );
    assert!(
        snap.primary_stale,
        "mainnet tip sits far below the freshest peer: primary must read stale"
    );
    assert_eq!(
        snap.healthy, 1,
        "exactly one peer eligible (the stale one steps aside)"
    );
    assert!(
        !pool.degraded(),
        "disagreement with an eligible peer is flagged, not a full outage"
    );
}
/// The background sampler end-to-end: spawned as in `main`, it must
/// report a reachable, healthy, non-degraded pool with measured latency.
#[tokio::test]
#[ignore]
async fn sampler_reports_real_health() {
    if std::env::var("RPC_POOL_ACCEPTANCE").as_deref() != Ok("1") {
        eprintln!("skipped: set RPC_POOL_ACCEPTANCE=1 to run the live pool probe");
        return;
    }
    let pool = RpcPool::build(vec![real_rpc_url()]);
    pool.spawn_sampler();
    tokio::time::sleep(std::time::Duration::from_secs(POOL_PROBE_SECS + 2)).await;

    let snap = pool.snapshot();
    eprintln!(
        "sampler: endpoints={} healthy={} stale={} inconsistent={} latencies_us={:?} probe_age={}s",
        snap.endpoints,
        snap.healthy,
        snap.primary_stale,
        snap.inconsistent,
        snap.latencies_us,
        snap.last_probe_age_secs
    );
    assert_eq!(snap.endpoints, 1);
    assert_eq!(snap.healthy, 1, "real endpoint eligible after probe");
    assert!(!pool.degraded(), "reachable pool is not degraded");
    assert!(
        snap.latencies_us[0] > 0,
        "sampler measured real latency (no scripted values here)"
    );
}
