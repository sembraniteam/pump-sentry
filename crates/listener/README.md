# listener — Pump.fun new-token event stream

Real-time detector for newly deployed Pump.fun tokens over Solana RPC.

## Architecture

```text
logsSubscribe (WebSocket, Mentions[pump_program])
  → filter: err.is_none() && logs contains "Instruction: Create"
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
  time-derived jitter.
- **Zero panics**: no `.unwrap()` / `.expect()` / unchecked indexing on hot
  paths; all failures map to `PumpParseError` / `ListenerError` (`thiserror`).
- **Zero-copy-ish parsing**: instruction args are Borsh-decoded from the byte
  slice in place; events are shared as `Arc<PumpTokenLaunchEvent>`.

## Pump.fun specifics

- Program: `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`
- `create` discriminator: `[24, 30, 200, 40, 5, 28, 7, 119]`
- `buy` discriminator: `[102, 6, 61, 18, 1, 218, 235, 234]`
- `sell` discriminator: `[51, 230, 133, 164, 1, 127, 131, 173]`

## Screening (`screen` module)

Win-rate hardening on top of the event stream:

- `extract_pump_swaps` classifies buy/sell instructions (trader = account
  index 6) in any fetched transaction.
- `EntryGate` is a pure watchlist state machine: entries get a time
  window to gather distinct non-dev buyers; dev sells abort immediately (dev dump), quorum confirms, silence times out.
- `screen_developer` checks dev-wallet history over RPC: rejects fresh
  wallets (no history) and serial launchers, fail-closed on transport
  errors. `judge_developer` holds the pure policy for unit tests.
- `RpcScreener` bundles both behind one client for the pipeline.
- Borsh args: `name: String, symbol: String, uri: String`
- Accounts: `[0]=mint, [2]=bonding_curve, [3]=associated_bonding_curve,
  [7]=user` (min 8).

Top-level instruction `data` is base58; inner-instruction `data` is tried as
base58 then base64. Account keys come from `transaction.message.accountKeys`
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
let config = ListenerConfig::default ();
let ( mut rx, stats, _handle) = PumpFunListener::subscribe(config).await?;
while let Some(ev) = rx.recv().await {
println!("{} ({}) {}", ev.name, ev.symbol, ev.mint);
}
// Unbounded variant: PumpFunListener::subscribe_unbounded(config).await?
```

## Tests

```sh
cargo test -p listener
```
