# listener — Pump.fun launch stream + screening

Real-time detector for new Pump.fun tokens: `logsSubscribe` → `getTransaction` → Borsh parse. Bounded everywhere, reconnects with backoff + gap backfill.

- Socket loop never blocks: 32 bounded fetch workers; channel 10k, drop-on-full with metrics.
- Stream recovery: reconnects with capped backoff; geyser backfills the slot gap under a 30s overall budget (20s per-slot cap), and past the slot cap or the budget the unread window is shed and counted as `slot_gaps_shed` (failed/pruned/timed-out blocks count too); WS pages the signature list up to 3 pages seeking the last seen signature (past the cap with unseen history beyond it counts `backfill_pages_capped` — lost, never silent).
- Geyser 429s trip a circuit breaker (30 consecutive): retries drop to hourly with a card, the heartbeat re-logs while banned; recovery is automatic on a working subscribe.
- Retries: 250ms transient + ~1s jittered tier for 429 (HTTP + JSON-RPC surfaces); permits never held across sleeps.
- `screen`: dev/creator history (reject fresh/serial, fail-closed; the dev walk is page-budgeted, so a truncated history fails closed instead of passing a lower-bound count), buyer-quorum watchlist (dev dump aborts), bundle forensics at confirm (reject drops, suspect paper-only). Confirm-time re-screens bypass the verdict cache, so a launch inside the TTL cannot ride a stale verdict.
- `screen::micro` (`MicroSignals`): per-round confirm-tape microstructure — buys/sells, buy ratio, base-unit volumes, unique traders, top-wallet share, insider net flow, round-over-round velocity. Pure computation over already-observed swaps: no new RPC. Round-over-round velocity joins one round's record to the stored previous point (`PendingSignal.prev_tape`).
- `Screener::top_holder_pct`: top-10 supply share read once per confirm for the armed cap and the risk model alike (unreadable refuses while armed; unarmed it is `None`, never zero).
- Funders resolve by walking to genesis (page-capped, deadline-bound); RPC caches: dev verdicts 1h (repeat screens only — confirm re-screens are always fresh), funder 24h (transient misses never cached).
- Zero panics; `create`/`create_v2` layouts frozen-sample proven.

```sh
cargo run -p listener --example watch   # prints live launches (mainnet)
cargo test -p listener
LISTENER_ACCEPTANCE=1 cargo test -p listener --lib backfill_pages_chain -- --ignored --nocapture  # $0 paging invariant vs real RPC
LISTENER_ACCEPTANCE=1 cargo test -p listener --lib live_socket_ingestion -- --ignored --nocapture  # $0 live socket smoke (~35s)
```
