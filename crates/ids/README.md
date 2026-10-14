# ids — single source of on-chain constants

Program IDs, discriminators, and fee/volume addresses. A program upgrade edits one file here — drift becomes a build break, not silent mainnet misbehavior.

- `pump`: program, authorities, all `create`/`buy`/`sell` discriminators (v1/v2/v3 — v3 is the only live dispatch shape).
- `raydium`: AMM v4 program, WSOL mint. `system`: system/SPL programs, rent, ATA.

```sh
cargo test -p ids
```
