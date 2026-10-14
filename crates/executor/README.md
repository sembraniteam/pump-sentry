# executor

Live Pump.fun buy/sell executor (v3 only). **Moves real funds** —
deliberately not wired into the paper binary. Fund a dedicated wallet,
review every parameter, and dry-run instruction inspection before
mainnet use.

## Architecture

```
WalletGuard::load_from_env (SOLANA_PRIVATE_KEY, zeroized buffer)
  → assert_solvency (route-aware: tip + reserve natively; trade funds
    from the WSOL ATA on SOL pairs)
  → PumpSwapBuilder::build_buy_v3_ix / build_sell_v3_ix (17 accounts:
    derived addresses + listed buyback pick, no fee recipient)
  → sign + simulateTransaction preflight + Jito bundle
```

- `wallet` — skill `solana-wallet-guard`: env-only secrets (64-byte
  secret or 32-byte seed), `MIN_SOL_RESERVE_LAMPORTS` floor, pre-flight
  solvency check with saturating math.
- `instructions` — skill `solana-pumpfun-swap` + live mainnet replays:
  v3 `buy_v3` / `sell_v3` (17 accounts, TRADE_V3.md), Borsh
  `(amount, limit)` args, program-bound ATA derivation (classic vs
  Token-2022 per mint owner). Retired shapes (legacy 12-account core,
  18-account mayhem, 27/26-account v2) are deleted: legacy fills are
  extinct and 18/v2 fills occur only on old curves, while every new
  launch is a v3 curve. SPL program IDs cross-checked against the
  `spl-token` crates in tests.
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
use executor::{BuyOrder, PumpSwapBuilder, SwapExecutor, WalletGuard, parse_pubkey};
use executor::instructions::BuyV3Accounts;
use solana_commitment_config::CommitmentConfig;

let executor = SwapExecutor::new(
    "https://api.mainnet-beta.solana.com",
    CommitmentConfig::confirmed(),
    WalletGuard::load_from_env()?,
)?;

let payer = parse_pubkey("PAYER")?;
let mint = parse_pubkey("MINT")?;
let quote_mint = parse_pubkey("So11111111111111111111111111111111111111112")?;
let token_program = parse_pubkey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")?;
let buy_v3 = PumpSwapBuilder::build_buy_v3_ix(
    1_000_000,            // base-token units to buy
    10_000_000,           // max quote spend: recomputed at dispatch via quote_buy_guard
    &BuyV3Accounts {
        payer: &payer,
        mint: &mint,
        quote_mint: &quote_mint,
        bonding_curve: &parse_pubkey("CURVE")?,
        associated_bonding_curve: &parse_pubkey("ASSOC")?,
        base_token_program: &token_program,
        quote_token_program: &token_program, // WSOL ATA owned by the classic program pre-wrap normalization
        buyback_recipient: &parse_pubkey("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD")?, // one of BUYBACK_FEE_RECIPIENTS (ids)
    },
)?;

let receipt = executor.buy(&BuyOrder {
    mint: parse_pubkey("MINT")?,
    bonding_curve: parse_pubkey("CURVE")?,
    associated_bonding_curve: parse_pubkey("ASSOC")?,
    amount_tokens: 1_000_000,      // base units
    max_sol_cost: 10_000_000,      // guard: recompute via internal quote_buy_guard
    tip_lamports: 0,               // Jito tip (used by buy_jito/sell_jito)
    creator: Some(parse_pubkey("CREATOR")?), // required: cross-checked vs curve state
    slippage_bps: 200,             // guard margin over the fee-inclusive quote
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
  failures refuse before fees burn; transport trouble proceeds with a log
  unless `SIM_STRICT=true` (`with_strict_sim`), which refuses unproven
  dispatches — enable it for live funds. One fresh-blockhash resample
  covers `BlockhashNotFound` under load.
- v3 needs no fee recipient (fees stay on the curve); only the buyback
  pick travels. Tip accounts refresh every 15 minutes from
  `getTipAccounts` with static fallback.
- Wallet funding is route-aware (official BUY.md/TRADE_V3.md): SOL-pair
  buys transfer native SOL, so the trade guard is asserted natively
  (trade + tip + 0.05 reserve) and no quote ATA is pre-created ([10] is
  unused and need not exist). Only token-paired quotes (USDC/pump coin)
  debit the payer quote ATA via `TransferChecked` — those need a funded
  quote float; simulation still decides sufficiency. Wrap at setup
  (`spl-token wrap <AMOUNT>`) only matters for token-paired inventory;
  preflight refuses natively-short SOL-pair buys before simulation.
- Unconfirmed buys are tracked as pending and re-polled, never assumed
  unfilled; sells retry with cooldown + attempt budget, then escalate.
  A crash-recovery WAL (`live-wal.jsonl`) replays pending/positions/
  manual bags on the next boot.
- Buy routing follows curve state (v3-only): creatorless orders,
  complete curves, cashback-era curves, and unreadable curve state all
  refuse fail-closed (loud diverged/manual upstream, never an unproven
  dispatch). Every buy recomputes its guard from a fee-inclusive
  on-chain quote (`quote` module: tier schedule + ceiling + slippage,
  fallback to the sized guard with a loud log) — the 6002 class is
  priced out before simulation, which still refuses the truly short
  remainder.
- Sell routing follows curve state, not the launch version: anything
  but a live creator v3 curve refuses fail-closed (exits belong to
  migration/manual); creator-less orders fail closed too.

## TODO before mainnet

- Mainnet tip calibration for `tip_lamports` under real bundle competition.
- Buy-shape gate must turn GREEN before funding entries: the 17-shape
  probe (`mainnet_v3_17_simulates`, both sides simulated clean).
