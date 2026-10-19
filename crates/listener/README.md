# listener — Pump.fun launch stream + screening

Real-time detector for new Pump.fun tokens: `logsSubscribe` → `getTransaction` → Borsh parse. Bounded everywhere, reconnects with backoff + gap backfill.

- Socket loop never blocks: 32 bounded fetch workers; channel 10k, drop-on-full with metrics.
- Stream recovery: reconnects with capped backoff; geyser backfills the slot gap (past the slot cap the window is shed and counted as `slot_gaps_shed`); WS pages the signature list up to 3 pages seeking the last seen signature (past the cap with unseen history beyond it counts `backfill_pages_capped` — lost, never silent).
- Geyser 429s trip a circuit breaker (30 consecutive): retries drop to hourly with a card, the heartbeat re-logs while banned; recovery is automatic on a working subscribe.
- Retries: 250ms transient + ~1s jittered tier for 429 (HTTP + JSON-RPC surfaces); permits never held across sleeps.
- `screen`: dev/creator history (reject fresh/serial, fail-closed), buyer-quorum watchlist (dev dump aborts), bundle forensics at confirm (reject drops, suspect paper-only).
- Funders resolve by walking to genesis (page-capped, deadline-bound); RPC caches: dev 1h, funder 24h (transient misses never cached).
- Zero panics; `create`/`create_v2` layouts frozen-sample proven.

```sh
cargo run -p listener --example watch   # prints live launches (mainnet)
cargo test -p listener
LISTENER_ACCEPTANCE=1 cargo test -p listener --lib backfill_pages_chain -- --ignored --nocapture  # $0 paging invariant vs real RPC
LISTENER_ACCEPTANCE=1 cargo test -p listener --lib live_socket_ingestion -- --ignored --nocapture  # $0 live socket smoke (~35s)
```
