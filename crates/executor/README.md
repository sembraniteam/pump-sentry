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
  Legacy 12-account core; creator curves route via the v3 fast path
  (17 accounts, derived + listed buyback), the 18-account mayhem tail
  ([16]/[17] from a same-curve reference), or 27-account `buy_v2`
  (opaque routing from a same-curve reference). The bare 16-account
  prefix is RETIRED as a dispatch shape (buyback-enforcing curves
  reject it with 6062) — it survives only as the shared builder body
  behind the 18-account construction and its byte-proof. SPL program
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
use executor::{BuyOrder, PumpSwapBuilder, SwapExecutor, WalletGuard, parse_pubkey};
use executor::instructions::{BuyV3Accounts, SwapKind};
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
let _ = SwapKind::Buy { amount: 1_000_000, max_sol_cost: 10_000_000, track_volume: true };

let receipt = executor.buy(&BuyOrder {
    mint: parse_pubkey("MINT")?,
    bonding_curve: parse_pubkey("CURVE")?,
    associated_bonding_curve: parse_pubkey("ASSOC")?,
    amount_tokens: 1_000_000,      // base units
    max_sol_cost: 10_000_000,      // guard: recompute via internal quote_buy_guard
    tip_lamports: 0,               // Jito tip (used by buy_jito/sell_jito)
    creator: None,                 // Some(..) selects the extended mayhem shape
    mayhem_ref_sig: None,          // gate hint for [16]/[17] (None = scan)
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
  dispatches — enable it for live funds.
- Fee recipient resolves live from the Global account (override with
  `FEE_RECIPIENT` / `with_fee_recipient`); tip accounts refresh hourly
  from `getTipAccounts` with static fallback.
- Wallet funding is two-part: native SOL (fees + solvency guard) AND a
  WSOL float in the wallet's WSOL ATA (every v3 buy debits WSOL via
  `TransferChecked` — a SOL-only wallet sim-fails with token
  `InsufficientFunds`, proven live). Wrap at setup
  (`spl-token wrap <AMOUNT>`) and top up as buys draw it down;
  preflight refuses with a "wrap SOL first" error when the float is
  short, and always idempotently pre-creates the payer quote ATA.
- Unconfirmed buys are tracked as pending and re-polled, never assumed
  unfilled; sells retry with cooldown + attempt budget, then escalate.
- Buy routing follows curve state: complete curves refuse; cashback
  coins use the v2 reference path (`BuyRouting`: 18-account or `buy_v2`
  from the curve's own buys, gate hint first); every other creator
  curve uses the v3 fast path (derived + listed buyback, no scan);
  creator-less orders keep the legacy core. Every buy recomputes its
  guard from a fee-inclusive on-chain quote (`quote` module: tier
  schedule + ceiling + slippage, fallback to the sized guard with a
  loud log) — the 6002 class is priced out before simulation, which
  still refuses the truly short remainder.
- Sell routing follows curve state, not the launch version: complete
  curves refuse without burning fees (exits belong to migration/manual);
  cashback coins build `sell_v2` from a same-curve reference sell
  (fail-closed when none scans); every other creator curve builds the
  17-account `sell_v3` from derived addresses plus the listed buyback;
  creator-less orders keep the legacy core.

## TODO before mainnet

- Mainnet tip calibration for `tip_lamports` under real bundle competition.
- Buy-shape gates must turn GREEN before funding entries: the 18-shape
  sim probe (`mainnet_mayhem_buy_18_simulates`), the 27-shape probe
  (`mainnet_buy_v2_27_simulates`), and the 17-shape probe
  (`mainnet_v3_17_simulates`).
  Routing, corrected Oct-2026 against docs + chain: the `[16]`/`[17]`
  tail is the universal post-Sept-1 buyback tail (mandatory on every
  curve — 6002 `TooMuchSolRequired` on the frozen failed sample is a
  1.15%-short guard, proven by its logs, NOT a shape error). Buyback
  vaults come from the 8 published wallets (on-chain Global array
  verified in order); `[16]` is per-curve unique — copy, never hardcode.
  Position `[18]` (v2 shapes) is the derived sharing-config PDA (bump
  255, proven on all four frozen v2 samples), not reference routing.
  Live router: v3 fast path (derived + listed buyback, no scan) unless
  cashback (v2 reference path) or complete (refuse); creator curves
  without state fall back to the v2 reference scan (`BuyRouting` picks
  18-account or `buy_v2` from whatever the curve's buyers used);
  absence fails closed (paper-only, never a guessed dispatch).
- `sell_v2` exits: builder byte-exact vs frozen sample + sim GREEN on a
  fresh curve (funds-conclusion at the pump balance gate — program walked
  the full layout to the debit math); opaque `[8]`/`[9]` still resolve
  per trade from a same-curve reference sell (fail-closed when none
  scans); `[18]` is the derived sharing-config PDA (proven bump 255 on
  both frozen samples).
