# executor

Live Pump.fun buy/sell executor. **Moves real funds** — deliberately not
wired into the paper binary. Fund a dedicated wallet, review every
parameter, and dry-run instruction inspection before mainnet use.

## Architecture

```
WalletGuard::load_from_env (SOLANA_PRIVATE_KEY, zeroized buffer)
  → assert_solvency(balance ≥ trade + tip + 0.05 SOL reserve)
  → PumpSwapBuilder::build_swap_ix (explicit SwapAccounts: live fee
    recipient + per-mint token program + creator; legacy build_buy_ix /
    build_sell_ix wrappers are deprecated — mayhem routing needs the full shape)
  → sign + send_and_confirm_transaction
```

- `wallet` — skill `solana-wallet-guard`: env-only secrets (64-byte
  secret or 32-byte seed), `MIN_SOL_RESERVE_LAMPORTS` floor, pre-flight
  solvency check with saturating math.
- `instructions` — skill `solana-pumpfun-swap` + live mainnet replays:
  buy `[102,6,61,18,1,218,235,234]` / sell `[51,230,133,164,1,127,131,173]`,
  Borsh `(amount, limit[, track_volume])` args, program-bound ATA derivation (classic vs Token-2022 per mint owner).
  Legacy 12-account core, plus an
  extended 16/14-account mayhem shape when the creator is known (creator
  vault + volume/fee-config routing, positions byte-verified). SPL program
  IDs cross-checked against the `spl-token` crates in tests.
- `executor` — `SwapExecutor::{buy,sell}` over standard RPC confirmation,
  plus `buy_jito` / `sell_jito` over Jito bundles when configured with
  `with_jito` (skill `jito-mev-execution`; wired into the live binary via
  `src/live.rs`). `tip_lamports` on the order funds the Jito tip.

## Version note

`solana-client 4.3` pairs with the `solana-transaction 4.x` /
`solana-keypair 3.x` / `solana-signer 3.x` type family, while
`solana-sdk 5` moved to `transaction 5.x`. This crate therefore builds
on the granular v3/v4 crates directly (not `solana-sdk`) so instruction,
keypair, and transaction types match the RPC client exactly. SPL
interoperability goes through raw `[u8; 32]` bytes for the same reason.

## Use

```rust
use executor::{BuyOrder, SwapExecutor, WalletGuard, parse_pubkey};
use solana_commitment_config::CommitmentConfig;

let executor = SwapExecutor::new(
    "https://api.mainnet-beta.solana.com",
    CommitmentConfig::confirmed(),
    WalletGuard::load_from_env()?,
)?;

let receipt = executor.buy(&BuyOrder {
    mint: parse_pubkey("MINT")?,
    bonding_curve: parse_pubkey("CURVE")?,
    associated_bonding_curve: parse_pubkey("ASSOC")?,
    amount_tokens: 1_000_000,      // base units
    max_sol_cost: 10_000_000,      // lamports
    tip_lamports: 0,               // Jito tip (used by buy_jito/sell_jito)
    creator: None,                 // Some(..) selects the extended mayhem shape
}).await?;
println!("confirmed: {}", receipt.signature);
```

## Test

```sh
cargo test -p executor
```

Live acceptance probe (ignored by default; costs $0 — unfunded payer):

```sh
JITO_ACCEPTANCE=1 cargo test -p executor --test jito_acceptance -- --ignored --nocapture
```

Verified live 2026-10-11: dummy bundle accepted by the mainnet block
engine (`result` UUID, no `-32602`), proving base64 decode with the
`{"encoding": "base64"}` param through the crate's exact wire path.

## Notes for live use

- Compute budget + priority fee attach via
  `with_compute_budget(limit, micro_lamports)` (`PRIORITY_FEE_MICRO_LAMPORTS`,
  `COMPUTE_UNIT_LIMIT`; both off by default).
- `sendBundle` carries `{"encoding": "base64"}` (live-verified; without it
  the engine refuses every bundle with `-32602`).
- Every dispatch runs `simulateTransaction` preflight first: definitive
  failures refuse before fees burn; transport trouble proceeds with a log.
- Fee recipient resolves live from the Global account (override with
  `FEE_RECIPIENT` / `with_fee_recipient`); tip accounts refresh hourly
  from `getTipAccounts` with static fallback.
- Unconfirmed buys are tracked as pending and re-polled, never assumed
  unfilled; sells retry with cooldown + attempt budget, then escalate.

## TODO before mainnet

- Mainnet tip calibration for `tip_lamports` under real bundle competition.
- Mayhem `[16]/[17]` buyback-routing sourcing (see `instructions` docs);
  the ignored `mainnet_layout_acceptance` probe is the gate.
- `sell_v2` discriminator/layout (currently unobserved on-chain).
