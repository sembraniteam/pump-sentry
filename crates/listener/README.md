# listener — Pump.fun launch stream + screening

Real-time detector for new Pump.fun tokens: `logsSubscribe` → `getTransaction` → Borsh parse. Bounded everywhere, reconnects with backoff + gap backfill.

- Socket loop never blocks: 32 bounded fetch workers; channel 10k, drop-on-full with metrics.
- Retries: 250ms transient + ~1s jittered tier for 429 (HTTP + JSON-RPC surfaces); permits never held across sleeps.
- `screen`: dev/creator history (reject fresh/serial, fail-closed), buyer-quorum watchlist (dev dump aborts), bundle forensics at confirm (reject drops, suspect paper-only).
- Funders resolve by walking to genesis (page-capped, deadline-bound); RPC caches: dev 1h, funder 24h (transient misses never cached).
- Zero panics; `create`/`create_v2` layouts frozen-sample proven.

```sh
cargo run -p listener --example watch   # prints live launches (mainnet)
cargo test -p listener
```
