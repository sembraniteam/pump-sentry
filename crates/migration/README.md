# migration — graduation + Raydium migration detection

Pure parsing, no I/O, no panics.

- Graduation: curve byte 48 set; progress from real SOL (84 = 100%).
- Classification: pump program/authority + `initialize2` log → RaydiumV4; `migrate` disc / `MigrateV2` log → PumpSwap.
- Pools: 752-byte slot reader, WSOL-**based** orientation chain-verified (base slot 400); trust only via `parse_validated` (Raydium owner + WSOL base + expected quote).
- Post-migration vault ticker: ≤50 pools per chunk, 15s per chunk under 60s overall.

```sh
cargo test -p migration
```
