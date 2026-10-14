# executor — live swaps (v3 only, moves real funds)

Guarded wallet, exact instruction builders, Jito bundles. Deliberately not wired into paper paths; only active with `--live`.

- Route-aware funding (official BUY.md/TRADE_V3.md): SOL-pair buys pay **native SOL** (guard asserted natively); token-paired quotes debit the quote ATA via `TransferChecked`.
- 17-account `buy_v3`/`sell_v3` from derived addresses + listed buyback; spend guard is the order ceiling (fresh quotes only lower it); `simulateTransaction` preflight gates every dispatch (`SIM_STRICT=true` for live).
- Retired shapes (legacy/18/mayhem/v2) deleted; cashback/complete/creatorless curves fail closed.
- Wallet: env-only secret (zeroized), 0.05 SOL floor; tips refresh ≤15min; pending buys re-polled, never assumed; crash WAL (`src/live.rs`, fsync'd) replays custody.

```sh
cargo test -p executor
JITO_ACCEPTANCE=1 cargo test -p executor --test jito_acceptance -- --ignored --nocapture  # $0 probe
```

Fund before live: native SOL for trade+fees+tips; WSOL float only for token-paired quotes. Prove the 17-shape gate (`mainnet_v3_17_simulates`) green first.
