# ids

Single source of truth for on-chain constants shared across crates:
program IDs, instruction discriminators, and fee/volume addresses.

A program upgrade or discriminator change edits exactly one file here —
every consumer re-exports from `ids`, so drift becomes a build break
instead of silent mainnet misbehavior.

- `pump` — Pump.fun program, fee recipient, global/event authorities,
  migration authority; `create` / `create_v2` / `buy` / `sell` /
  `buy_v2` / `migrate` discriminators (v2 + `sell_v2` gaps documented
  where unverified).
- `raydium` — Raydium AMM v4 program, WSOL mint.
- `system` — system program, SPL token programs, rent sysvar, ATA program.

## Test

```sh
cargo test -p ids
```
