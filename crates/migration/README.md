# migration

Pump.fun graduation detector and Raydium migration classifier (skill
`solana-raydium-migration`). Pure parsing: no I/O, no panics.

## Architecture

```
bonding-curve bytes ──→ is_curve_graduated (byte 48 != 0; any nonzero
                         counts, matching the curve parser)
real SOL reserves   ──→ calculate_progress (84 SOL = 100%)
migration tx        ──→ classify_migration:
                         pump program or authority 39az…Jjg present, then
                         Raydium program + `initialize2` log (case-
                         insensitive) → RaydiumV4,
                         `migrate` discriminator or `MigrateV2` log → PumpSwap
fresh pool account  ──→ RaydiumV4Pool::parse (AMM ID + base/quote
                         mints + both LP vaults + LP mint)
```

- `detector` — `MigrationMonitor` with the canonical addresses,
  graduation flag/progress, and destination classification. Errors via
  `thiserror` (`MigrationError`).
- `pool` — `LiquidityStateV4` slot reader (752-byte account; quote mint
  at offset 432, contiguous 32-byte slots for vaults/mints). Tolerates
  longer buffers for forward compatibility; `is_wsol_quoted` flags
  standard Pump.fun migrations; `validate` gates trust (Raydium owner,
  WSOL quote, expected base mint). `parse` alone is untrusted by
  contract — use `parse_validated` (parse + gate in one step).
- `vault` — post-migration price ticker: chunked batched vault-balance
  reads (≤50 pools per round, 15s deadline) converted to USD spot (Pump 6dp / WSOL 9dp).

## Use

```rust
use migration::{MigrationMonitor, RaydiumV4Pool};

assert!(MigrationMonitor::is_curve_graduated(&account_data));
assert_eq!(MigrationMonitor::calculate_progress(42_000_000_000), 50.0);

let pool = RaydiumV4Pool::parse(&amm_id, &pool_data)?;
assert!(pool.is_wsol_quoted());
```

## Test

```sh
cargo test -p migration
```
