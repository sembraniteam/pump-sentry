//! Watch Pump.fun launches on mainnet and print each event.
//!
//! Run with:
//! ```sh
//! cargo run -p listener --example watch
//! ```

use listener::{DEFAULT_HTTP_RPC_URL, DEFAULT_WS_RPC_URL, ListenerConfig, PumpFunListener};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    // Same dual-provider rationale as the main binary (WSS below).
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("info,listener=debug"))
        .init();

    let config = ListenerConfig {
        ws_url: DEFAULT_WS_RPC_URL.to_string(),
        http_url: DEFAULT_HTTP_RPC_URL.to_string(),
        commitment: "confirmed".to_string(),
    };

    let (mut rx, stats, _handle) = match PumpFunListener::subscribe(config).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("failed to subscribe: {e}");
            std::process::exit(1);
        }
    };

    println!("watching pump.fun launches (Ctrl-C to stop)…");
    let mut seen: u64 = 0;
    while let Some(ev) = rx.recv().await {
        seen += 1;
        println!(
            "[#{seen}] {} ({}) mint={} user={} slot={} sig={}",
            ev.name, ev.symbol, ev.mint, ev.user, ev.slot, ev.signature
        );
        println!(
            "      bonding_curve={} associated={}",
            ev.bonding_curve, ev.associated_bonding_curve
        );
        println!("      uri={}", ev.uri);
        let snap = stats.snapshot();
        println!(
            "      stats logs={} candidates={} fetch_ok={} parse_ok={} dropped={}",
            snap.logs_seen, snap.candidates, snap.fetch_ok, snap.parse_ok, snap.dropped_full
        );
    }
}
