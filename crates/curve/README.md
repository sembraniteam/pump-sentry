# curve

Pump.fun bonding-curve reserve reader and polling price ticker.
Turns on-chain virtual reserves into USD spot prices so entries and
TP/SL exits are fully dynamic — live Jupiter SOL/USD feed only
(`QUOTE_USD` survives solely as a paper-only test override, never for
live sizing).

## Architecture

```
getAccount / getMultipleAccounts (HTTP RPC)
  → layout::BondingCurveReserves::parse (8-byte disc + 5×u64 + complete)
  → price_sol = virtual_sol / virtual_token × 10^decimals / 1e9
  → price_usd = price_sol × sol_usd (live Jupiter feed, never the retired SOL_USD env)
  → liquidity_usd ≈ virtual-SOL side in USD (buy-side depth proxy)
```

- `layout` — pure parser + price math, verified against the Pump.fun
  IDL shape (`virtual_token_reserves`, `virtual_sol_reserves`,
  `real_*`, `token_total_supply`, `complete`); genesis-reserve unit
  test pins spot ≈ 2.7959e-8 SOL.
- `sol_feed` — shared Jupiter SOL/USD feed, 120s stale-gated: `price()`
  returns `None` past the deadline and every consumer (ticker rounds,
  live buy/sell sizing) fails closed instead of printing stale USD.
- `ticker` — `CurveTicker` with a dynamic watch set (`watch`/`unwatch` per mint), `fetch_tick` one-shot reads, and a
  `run` loop emitting `PriceTick`s via chunked `getMultipleAccounts` (≤100
  accounts per call) per round. Every round is deadline-bound (`ROUND_TIMEOUT` 15s); missed rounds log and continue; the
  loop ends
  when the receiver drops. Unknown commitments are rejected (`None`),
  never coerced — callers fail closed.
- `token_decimals` — mint decimals from the mint account (offset 44,
  shared by both SPL programs), cached per mint and fail-closed on
  transport errors. Ticks price with real decimals (no 10^Δ misprints);
  the pipeline's confirm gate still drops non-6 mints rather than
  dispatching them.

## Use

```rust
use curve::{CurveTicker, SolUsdFeed};
use solana_commitment_config::CommitmentConfig;
use std::sync::Arc;
use std::time::Duration;

let sol_feed = SolUsdFeed::with_defaults();
let _poller = sol_feed.clone().spawn(); // background refresher, abort on shutdown
let ticker = CurveTicker::new(
    "https://api.mainnet-beta.solana.com",
    CommitmentConfig::confirmed(),
    Duration::from_millis(1000),
    Arc::clone(&sol_feed),
)?;
ticker.watch("MINT", "CURVE_ADDRESS").await?;

let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
tokio::spawn(ticker.run(tx));
while let Some(tick) = rx.recv().await {
    println!("{} = ${:.10} (liq ${:.0})", tick.mint, tick.price_usd, tick.liquidity_usd);
}
```

## Test

```sh
cargo test -p curve
```
