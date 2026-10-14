---
description: Execute a manual test buy transaction on Pump.fun using live SOL ($1 Mint Address, $2 Amount in SOL).
agent: solana-swap-executor
subtask: true
---

Execute an on-chain test buy swap against Pump.fun (moves REAL funds — dry-run `simulateTransaction` first and
re-read `crates/executor/README.md` before enabling):

- **Target Mint:** $1
- **Amount (SOL):** $2

### Verification Invariants (mirror `src/live.rs` + `crates/executor`):

1. Confirm wallet balance >= ($2 SOL + 0.05 SOL reserve + Jito tip) via `assert_solvency`.
2. Resolve the mint's token program (classic vs Token-2022) and derive the program-bound ATA; prepend the idempotent
   create-ATA ix when missing.
3. Size via `plan_buy` (USD notional at curve spot, `max_sol_cost` capped by `slippage_bps`; slippage ≥ 100%
   is rejected fail-closed, default 200 bps) and build via `build_buy_v3_ix` with explicit `BuyV3Accounts`
   (per-mint token programs + buyback recipient).
   NEVER use a retired builder (`build_buy_ix` / `build_swap_ix` / `SwapAccounts` do not exist on v3).
4. Dispatch as a Jito bundle (`sendBundle` + `{"encoding": "base64"}`) with 30s dispatch deadline; confirm the
   signature within 60s. Timeout/transport failures track as pending and re-poll — never assume unfilled.
5. Sign with `SOLANA_PRIVATE_KEY` and dispatch bundle.

### Output Contract:

```text
=== LIVE BUY TRANSACTION RECEIPT ===
Target Mint:      $1
SOL Dispatched:   $2 SOL
Jito Tip:         <tip_lamports> lamports
Bundle ID:        <bundle_uuid>
Signature:        <transaction_signature>
Explorer URL:     https://explorer.solana.com/tx/<signature>
Inclusion Status: [CONFIRMED | PENDING_RECHECK | FAILED]
====================================
```
