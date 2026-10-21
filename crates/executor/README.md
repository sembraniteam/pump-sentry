# executor — live swaps (v3 only, moves real funds)

Guarded wallet, exact instruction builders, Jito bundles. Not wired into paper paths; only active with `--live`.

- SOL-pair buys pay **native SOL**; token-paired quotes debit the quote ATA.
- 17-account `buy_v3`/`sell_v3` from derived addresses + listed buyback; order ceiling is the spend guard; sim preflight gates every dispatch (`SIM_STRICT=true` for live).
- Legacy/18/v2 shapes deleted; cashback/complete/creatorless curves fail closed.
- Wallet: env-only secret (zeroized), 0.05 SOL floor.
- Tips: the account list (15min TTL) and the landed-tip floor (60s TTL) are refreshed by
  background tasks; dispatch reads the cache only, so a stalled oracle costs freshness,
  never send latency.

## Read-path redundancy (`rpc_pool`)

The one shared `RpcClient` wraps `RpcPool`: sequential failover across
endpoints (never an all-endpoints broadcast), per-endpoint latency
tracking, and a background sampler that doubles as health check and
slot-staleness/inconsistency detector. Attempt budgets are per-method
(6s light / 25s block-weight) and sit under the callers' 10s outers, so
a hung primary actually fails over. `degraded()` is hysteresis-gated —
sampler-confirmed only, never a single blip (a lone cooldown is a
read-level fail-closed, not a pause).

No dispatch fan-out, ever: the pool hardens READS (balances, confirms,
blockhashes, quotes) only. Order execution stays single-path Jito;
executed-once semantics belong to the send/confirm guards, not to
redundancy. Configure fallbacks via `mainnet_rpc_urls_fallback` (config file only — the
environment is not read for tunables); empty keeps primary-only. Boot trust-checks every
endpoint: malformed/non-http URLs, remote plain http, and cross-cluster fallbacks refuse a
live start.

```sh
cargo test -p executor
RPC_POOL_ACCEPTANCE=1 cargo test -p executor --test rpc_pool_acceptance -- --ignored --nocapture  # $0 read-only pool proof (dead-primary failover + sampler vs real RPC)
JITO_ACCEPTANCE=1 cargo test -p executor --test jito_acceptance -- --ignored --nocapture  # $0 bundle proof
MAINNET_LAYOUT=1 cargo test -p executor --test mainnet_layout_acceptance -- --ignored --nocapture  # $0 v3 sim proof
RAYDIUM_ACCEPTANCE=1 cargo test -p executor --test raydium_authority_acceptance -- --ignored --nocapture  # $0 exit proof
# creatorless-fee probe (skips itself without PUMP_ACCEPTANCE_POOL):
MAINNET_LAYOUT=1 PUMP_ACCEPTANCE_POOL=<pool> [PUMP_ACCEPTANCE_HOLDER=<token acct>] \
  cargo test -p executor --test mainnet_layout_acceptance -- --ignored --nocapture creatorless
```

All four probes are simulation-only ($0 by construction: unfunded
payers, nothing sent); rerun on throttled/stale samples. Probes dial
`MAINNET_RPC_URL` when set (dedicated RPC recommended: the public node
429s after bulk scans). The creatorless probe needs `PUMP_ACCEPTANCE_POOL`
(a WSOL-quoted PumpSwap pool with default `coin_creator`; known-good
2026-10-19: `7h4HzxjKWzKYddmsEjLfoqWYqGqC5cHats6W7D9ZF6t6`
(net 175292, gap 531 ≈ the 30 bps tier) — early-era pools carry default
creators while active ones set them; resample
(`BqSFJWicNwh7PfppBqsmzxinDuJyqmunwfEv6fD4zhvK` also green with teeth;
`XVJ99qcNErahwPWqN6j2u1za27Pq1pTzDjYmznXtcwW` greens vacuously, net 18
lamports collapses both floors — avoid);
`PUMP_ACCEPTANCE_HOLDER` names a funded base-side token account only for
legacy pools whose trades predate public signature windows (otherwise a
holder is found automatically). None of these variables is read by the
bot.

Fund before live: native SOL for trade+fees+tips; WSOL float only for token-paired quotes.
