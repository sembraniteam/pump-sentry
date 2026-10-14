# ids

Single source of truth for on-chain constants shared across crates:
program IDs, instruction discriminators, and fee/volume addresses.

A program upgrade or discriminator change edits exactly one file here —
every consumer re-exports from `ids`, so drift becomes a build break
instead of silent mainnet misbehavior.

- `pump` — Pump.fun program, fee recipient, global/event authorities,
  migration authority; `create` / `create_v2` / `buy` / `sell` /
  `buy_v2` / `sell_v2` / `buy_v3` / `sell_v3` /
  `buy_exact_quote_in_v3` / `migrate` discriminators (all
  mainnet-verified; see the frozen fixtures in `listener`/`migration`).
  The v3 trio is the only live dispatch shape — the rest serve
  screening/quorum on historical fills.
- `raydium` — Raydium AMM v4 program, WSOL mint.
- `system` — system program, SPL token programs, rent sysvar, ATA program.

## Test

```sh
cargo test -p ids
```
