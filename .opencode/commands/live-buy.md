---
description: Execute a manual test buy transaction on Pump.fun using live SOL ($1 Mint Address, $2 Amount in SOL).
agent: solana-swap-executor
subtask: true
---

Execute an on-chain test buy swap against Pump.fun:

- **Target Mint:** $1
- **Amount (SOL):** $2

### Verification Invariants:

1. Confirm wallet balance >= ($2 SOL + 0.05 SOL reserve + Jito tip).
2. Construct `create_associated_token_account_idempotent` instruction.
3. Construct `PumpSwapBuilder::build_buy_ix` with 300 bps (3%) slippage cap.
4. Prepend priority fee and append Jito tip instruction.
5. Sign with `SOLANA_PRIVATE_KEY` and dispatch bundle.

### Output Contract:

```text
=== LIVE BUY TRANSACTION RECEIPT ===
Target Mint:      $1
SOL Dispatched:   $2 SOL
Jito Tip:         0.001 SOL
Signature:        <transaction_signature>
Explorer URL:     https://solscan.io/tx/<signature>
Inclusion Status: [CONFIRMED | FAILED]
====================================
```