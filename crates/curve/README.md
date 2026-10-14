# curve — bonding-curve reserves + price ticker

Turns on-chain virtual reserves into live USD spot: no static prices anywhere (`QUOTE_USD` is paper-tests only).

- Parser pinned to the Pump.fun IDL shape; prices with real mint decimals; confirm gate drops non-6 mints.
- Jupiter SOL/USD feed, 120s stale-gated: stale reads fail closed, never print old USD.
- Ticker: dynamic watch set, chunked `getMultipleAccounts` (≤100), 15s per chunk under a 60s round budget; missed rounds log and continue.

```sh
cargo test -p curve
```
