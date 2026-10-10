---
description: Run on-chain and RugCheck security audit against a token mint address.
agent: token-security-auditor
subtask: true
---

Audit the target Solana token mint address:
**Target Mint:** $ARGUMENTS

Current git branch:
!`git branch --show-current`

### Verification Tasks:

1. Verify if `mint_authority` and `freeze_authority` are disabled.
2. Query RugCheck risk score and list all detected flags.
3. Check top 10 non-bonding-curve holder distribution.

### Expected Output Format:

| Metric           | Status              | Value               | Risk Rating            |
|:-----------------|:--------------------|:--------------------|:-----------------------|
| Mint Authority   | [Disabled / Active] | `<address_or_null>` | [SAFE / CRITICAL]      |
| Freeze Authority | [Disabled / Active] | `<address_or_null>` | [SAFE / CRITICAL]      |
| RugCheck Score   | [Computed]          | `<score>/1000`      | [SAFE / WARN / DANGER] |
| Top 10 Holder %  | [Computed]          | `<pct>%`            | [SAFE / DANGER]        |

**Decision:** `[PROCEED_TO_SNIPE | REJECT_TOKEN]`
**Reason:** `<brief_justification>`
