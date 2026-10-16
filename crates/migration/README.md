# migration — graduation + migration detection (Raydium legacy, PumpSwap live)

Pure parsing, no I/O, no panics.

- Graduation: curve byte 48 set; progress from real SOL (84 = 100%).
- Classification: pump program/authority + `initialize2` log → RaydiumV4; `migrate` disc / `MigrateV2` log → PumpSwap.
- Pools: Raydium 752-byte slot reader (WSOL-**based**); PumpSwap `Pool` reader (token at BASE, WSOL at QUOTE, signed virtual reserves, length-tolerant tail); trust only via `parse_validated`. Frozen proofs cover both paths: a depth-2 quote (fail-closed pin) and a WSOL-quoted executable pool (replay-pinned).
- Post-migration vault ticker: ≤50 pools per chunk, 15s per chunk under 60s overall; PumpSwap legs add the pool account for effective-quote pricing.

```sh
cargo test -p migration
```
