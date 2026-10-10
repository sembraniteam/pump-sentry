---
description: Query on-chain SOL balance, gas reserve, and active SPL token accounts from the configured keypair.
agent: solana-swap-executor
subtask: true
---

Check the on-chain status of the live trading wallet configured in `SOLANA_PRIVATE_KEY`:

### Tasks:

1. Verify that `SOLANA_PRIVATE_KEY` is present and valid Base58.
2. Query RPC for native SOL balance and evaluate reserve health (minimum 0.05 SOL required).
3. Query all SPL token accounts owned by this wallet on Pump.fun / Solana mainnet.

### Output Contract:

```text
=== WALLET ON-CHAIN HEALTH CHECK ===
Public Key:      <base58_pubkey>
SOL Balance:     <balance_in_sol> SOL
Gas Reserve:     [HEALTHY (>= 0.05 SOL) | DEFICIT (< 0.05 SOL)]
Solvent for Buy: [YES | NO]
Active Tokens:
  - Mint: <mint_1> | Balance: <amount> | Status: [HOLDING]
====================================
```