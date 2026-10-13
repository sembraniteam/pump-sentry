# listener — Pump.fun new-token event stream

Real-time detector for newly deployed Pump.fun tokens over Solana RPC.

## Architecture

```text
logsSubscribe (WebSocket, Mentions[pump_program])
  → filter: err.is_none() && logs contains "Instruction: Create[V2]"
  → spawn worker: getTransaction (HTTP RPC, JSON) → parse → mpsc(10_000)
  → consumer prints / scores / buys
```

- **Socket read loop never blocks**: it only filters log notifications and
  spawns bounded-concurrency workers (`MAX_CONCURRENT_FETCHES = 32`) for the
  `getTransaction` round-trips.
- **Backpressure**: downstream is `tokio::sync::mpsc::channel(10_000)`.
  `try_send` on full drops the event and bumps `dropped_full` — the loop
  never awaits a full queue.
- **Reconnects**: exponential backoff, 100ms base, 5s cap, plus 0–100ms
  time-derived jitter. Every reconnect (not the first boot) backfills
  the socket gap: latest 100 program signatures replay oldest-first
  through the same workers, deduplicated by a 4,000-entry seen-set.
- **Fetch retries**: one 250ms retry for transient failures, plus a
  ~1s jittered third attempt for `429` alone (the fast retry usually
  lands inside the same limit window). Permits are re-acquired per
  attempt, never held across backoff sleeps.
- **RPC caches**: dev verdicts (`DEV_CACHE_TTL_SECS` = 1h) and funder
  ancestry (`FUNDER_CACHE_TTL_SECS` = 24h) sit in bounded TTL maps, so
  repeat wallets never re-burn `getSignaturesForAddress` pages.
- **Zero panics**: no `.unwrap()` / `.expect()` / unchecked indexing on hot
  paths; all failures map to `PumpParseError` / `ListenerError` (`thiserror`).
- **Zero-copy-ish parsing**: instruction args are Borsh-decoded from the byte
  slice in place; events are shared as `Arc<PumpTokenLaunchEvent>`.

## Pump.fun specifics

- Program: `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`
- `create` discriminator: `[24, 30, 200, 40, 5, 28, 7, 119]`
- `create_v2` discriminator: `[214, 144, 76, 236, 95, 139, 49, 180]` (14-account layout; creator parsed from args)
- `buy` discriminator: `[102, 6, 61, 18, 1, 218, 235, 234]`
- `sell` discriminator: `[51, 230, 133, 164, 1, 127, 131, 173]`
- `buy_v2` discriminator: `[184, 23, 238, 97, 103, 197, 211, 61]` (27 accounts,
  trader = fee payer at index 13, amount = first u64 — dual-sample proven)
- `sell_v2` discriminator: `[93, 246, 130, 60, 231, 233, 64, 178]` (26 accounts,
  trader = fee payer at index 13, amount = first u64 — frozen-sample proven;
  dev dumps through it abort the gate like legacy sells)

## Screening (`screen` module)

Win-rate hardening on top of the event stream:

- `extract_pump_swaps` classifies buy/sell instructions (legacy trader =
  account index 6; v2 `buy_v2`/`sell_v2` trader = fee payer at index 13,
  amounts = first u64 — both frozen-sample proven) in any fetched
  transaction. Unmapped pump instructions surface as `Unknown` (first
  account, amount 0, never quorum-counted).
- `EntryGate` is a pure watchlist state machine: entries get a time
  window to gather distinct non-dev buyers; sells (or unknown ixs) from
  any watched insider abort immediately (dev dump), quorum confirms, silence times out.
  v2 launches watch a 2-wallet cohort (launcher + distinct creator) via
  `add_cohort`; cohort buys never count.
- `screen_developer` checks dev-wallet history over RPC: rejects fresh
  wallets (no history) and serial launchers, fail-closed on transport
  errors. `judge_developer` holds the pure policy for unit tests.
  Prior-launch counting (`count_dev_creates`) uses the versioned author
  index (v1: 7, v2: 5) so v2 serial launchers cannot hide. The pipeline
  screens the v2 creator too and carries the worst of both trusts.
- `RpcScreener` bundles both behind one client for the pipeline.
- `BundleDetector::evaluate_launch_slot` (skill `sybil-cluster-detection`):
  3+ buys in the exact creation slot, or 2+ wallets sharing one genesis
  funder, proves a developer bundle; over 15% creation-slot supply
  escalates to reject. Pure function over `(slot, buyer, supply_pct)`
  swaps plus `(buyer, funder)` ancestry pairs.
- `RpcScreener::bundle_verdict` runs the full forensics at confirm time:
  recent curve swaps with slots and supply shares (from decoded amounts
  over curve total supply) plus genesis-funder tracing for the first few
  distinct buyers. Funders resolve by walking each wallet's history to
  its first transaction — page-capped (`MAX_FUNDER_PAGES`), system
  `transfer` / `createAccount` / seeded variants (incl. inner/CPI-routed)
  plus payer-paid ATA creation — with an 8s per-buyer deadline inside a
  15s outer budget, fanned out concurrently so one deep history cannot
  stall the gate. Untraceable majorities escalate to suspect (fail-closed).
- Borsh args: `name: String, symbol: String, uri: String` (v2 trailing args ignored by design)
- Accounts v1: `[0]=mint, [2]=bonding_curve, [3]=associated_bonding_curve,
  [7]=user` (min 8). Accounts v2: mint/bonding-curve/assoc at `[0]/[2]/[3]`,
  user at `[5]` (min 14).

Instruction `data` decodes through one shared helper (base58, then standard
and URL-safe base64) for top-level and inner instructions alike. Account keys come from
`transaction.message.accountKeys`
plus `meta.loadedAddresses` (writable, then readonly).

> Note: the fetch uses `UiTransactionEncoding::Json` because that is the
> encoding that returns the structured `accountKeys` / compiled-instruction
> form the parser consumes. Raw binary (`Base64`) payloads are explicitly
> rejected with a typed error rather than guessed at.

## Run the example

```sh
cargo run -p listener --example watch
```

Connects to mainnet (`wss(s)://api.mainnet-beta.solana.com`, commitment
`confirmed`) and prints every launch:

```text
[#1] Pepe (PEPE) mint=… user=… slot=… sig=…
      bonding_curve=… associated=…
      uri=https://…
```

## API sketch

```rust
let config = ListenerConfig::default();
let ( mut rx, stats, _handle) = PumpFunListener::subscribe(config).await?;
while let Some(ev) = rx.recv().await {
println!("{} ({}) {}", ev.name, ev.symbol, ev.mint);
}
// (No unbounded variant exists: every subscription is bounded + drop-on-full.)
```

## Tests

```sh
cargo test -p listener
```
