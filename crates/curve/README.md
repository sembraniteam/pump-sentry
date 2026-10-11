# curve

Pump.fun bonding-curve reserve reader and polling price ticker.
Turns on-chain virtual reserves into USD spot prices so entries and
TP/SL exits are fully dynamic — no more `QUOTE_USD`.

## Architecture

```
getAccount / getMultipleAccounts (HTTP RPC)
  → layout::BondingCurveReserves::parse (8-byte disc + 5×u64 + complete)
  → price_sol = virtual_sol / virtual_token × 10^decimals / 1e9
  → price_usd = price_sol × SOL_USD
  → liquidity_usd ≈ virtual-SOL side in USD (buy-side depth proxy)
```

- `layout` — pure parser + price math, verified against the Pump.fun
  IDL shape (`virtual_token_reserves`, `virtual_sol_reserves`,
  `real_*`, `token_total_supply`, `complete`); genesis-reserve unit
  test pins spot ≈ 2.7959e-8 SOL.
- `ticker` — `CurveTicker` with a dynamic watch set (`watch`/`unwatch` per mint), `fetch_tick` one-shot reads, and a
  `run` loop emitting `PriceTick`s via chunked `getMultipleAccounts` (≤100
  accounts per call) per round. Every round is deadline-bound (`ROUND_TIMEOUT` 15s); missed rounds log and continue; the
  loop ends
  when the receiver drops. Unknown commitments are rejected (`None`),
  never coerced — callers fail closed.
- `token_decimals` — mint decimals from the mint account (offset 44,
  shared by both SPL programs), cached per mint and fail-closed on
  transport errors. The pipeline's confirm gate drops non-6 mints
  rather than mispricing base-unit math by 10^Δ.

## Use

```rust
use curve::CurveTicker;
use solana_commitment_config::CommitmentConfig;
use std::time::Duration;

let ticker = CurveTicker::new(
    "https://api.mainnet-beta.solana.com",
    CommitmentConfig::confirmed(),
    Duration::from_millis(1000),
    150.0, // SOL_USD
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
