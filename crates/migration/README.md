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
- `pool` — `LiquidityStateV4` slot reader (752-byte account; contiguous
  32-byte slots for vaults/mints at 336/368/400/432/464, pinned by two
  frozen mainnet pools). Orientation is chain-verified: Pump.fun
  migration pools are WSOL-**based** (WSOL at slot 400, token at 432);
  `is_wsol_based` flags them; `validate` gates trust (Raydium owner,
  WSOL base, expected token quote). `parse` alone is untrusted by
  contract — use `parse_validated` (parse + gate in one step).
- `vault` — post-migration price ticker: chunked batched vault-balance
  reads (≤50 pools per round, 15s deadline) converted to USD spot
  (SOL-per-token from the WSOL base vault over the token quote vault;
  WSOL 9dp / Pump 6dp).

## Use

```rust
use migration::{MigrationMonitor, RaydiumV4Pool};

assert!(MigrationMonitor::is_curve_graduated(&account_data));
assert_eq!(MigrationMonitor::calculate_progress(42_000_000_000), 50.0);

let pool = RaydiumV4Pool::parse(&amm_id, &pool_data)?;
assert!(pool.is_wsol_based());
```

## Test

```sh
cargo test -p migration
```
