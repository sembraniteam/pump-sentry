# executor

Live Pump.fun buy/sell executor. **Moves real funds** — deliberately not
wired into the paper binary. Fund a dedicated wallet, review every
parameter, and dry-run instruction inspection before mainnet use.

## Architecture

```
WalletGuard::load_from_env (SOLANA_PRIVATE_KEY, zeroized buffer)
  → assert_solvency(balance ≥ trade + tip + 0.05 SOL reserve)
  → PumpSwapBuilder::build_buy_ix / build_sell_ix (skill discriminators)
  → sign + send_and_confirm_transaction
```

- `wallet` — skill `solana-wallet-guard`: env-only secrets (64-byte
  secret or 32-byte seed), `MIN_SOL_RESERVE_LAMPORTS` floor, pre-flight
  solvency check with saturating math.
- `instructions` — skill `solana-pumpfun-swap`: buy
  `[102,6,61,18,1,218,235,234]` / sell `[51,230,133,164,1,127,131,173]`,
  exact 12-account order, Borsh `(amount, limit)` args, ATA derivation.
  SPL program IDs cross-checked against the `spl-token` crates in tests.
- `executor` — `SwapExecutor::{buy,sell}` over standard RPC confirmation.

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
    tip_lamports: 0,               // reserved; not routed until Jito lands
}).await?;
println!("confirmed: {}", receipt.signature);
```

## Test

```sh
cargo test -p executor
```

## TODO before mainnet

- Jito bundle routing for `tip_lamports` (skill `jito-mev-execution`).
- Priority-fee / compute-budget instructions under congestion.
- Simulation (`simulateTransaction`) pre-flight before sending.
