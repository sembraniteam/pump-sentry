# executor — live swaps (v3 only, moves real funds)

Guarded wallet, exact instruction builders, Jito bundles. Not wired into paper paths; only active with `--live`.

- SOL-pair buys pay **native SOL**; token-paired quotes debit the quote ATA.
- 17-account `buy_v3`/`sell_v3` from derived addresses + listed buyback; order ceiling is the spend guard; sim preflight gates every dispatch (`SIM_STRICT=true` for live).
- Legacy/18/v2 shapes deleted; cashback/complete/creatorless curves fail closed.
- Wallet: env-only secret (zeroized), 0.05 SOL floor; tips refresh ≤15min.

```sh
cargo test -p executor
JITO_ACCEPTANCE=1 cargo test -p executor --test jito_acceptance -- --ignored --nocapture  # $0 probe
```

Fund before live: native SOL for trade+fees+tips; WSOL float only for token-paired quotes.
