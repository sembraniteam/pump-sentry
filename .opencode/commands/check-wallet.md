---
description: Query on-chain SOL balance, gas reserve, and active SPL token accounts from the configured keypair.
agent: solana-swap-executor
subtask: true
---

Check the on-chain status of the live trading wallet configured in `SOLANA_PRIVATE_KEY`:

### Tasks (mirror `crates/executor/src/wallet.rs` — key is base58 64-byte secret OR 32-byte seed):

1. Verify that `SOLANA_PRIVATE_KEY` is present and valid Base58 (64-byte secret or 32-byte seed; buffers zeroized).
2. Query RPC for native SOL balance (15s deadline) and evaluate reserve health (minimum 0.05 SOL required).
3. Solvent for a trade means `balance >= trade_lamports + tip_lamports + 0.05 SOL` (saturating math); sells still
   enforce reserve + tip. Also verify the wallet actually holds the tokens before sizing a sell (underfunded ATAs
   fail on-chain after burning fees).

### Output Contract:

```text
=== WALLET ON-CHAIN HEALTH CHECK ===
Public Key:      <base58_pubkey>
SOL Balance:     <balance_in_sol> SOL
Gas Reserve:     [HEALTHY (>= 0.05 SOL) | DEFICIT (< 0.05 SOL)]
Solvent for Buy: [YES | NO]
====================================
```
