---
description: Run on-chain and RugCheck security audit against a token mint address.
agent: token-security-auditor
subtask: true
---

Audit the target Solana token mint address:
**Target Mint:** $ARGUMENTS

Current git branch:
!`git branch --show-current`

### Verification Tasks (mirror `crates/auditor` policy — anything else is advisory only):

1. Verify `mint_authority` and `freeze_authority` are disabled (absent or blank).
2. Query RugCheck risk score (reject > 400) and list all flags (reject `danger` AND any unknown/empty severity —
   only `warn` passes).
3. Confirm the report carries a score and its mint matches the request (fail-closed otherwise).
4. For live-book context, also report the dev screen (`screen_developer`: age + prior launches) and the bundle
   verdict tier. This bot does NOT do top-holder distribution analysis — do not present one.

### Expected Output Format:

| Metric           | Status              | Value               | Risk Rating       |
|:-----------------|:--------------------|:--------------------|:------------------|
| Mint Authority   | [Disabled / Active] | `<address_or_null>` | [SAFE / CRITICAL] |
| Freeze Authority | [Disabled / Active] | `<address_or_null>` | [SAFE / CRITICAL] |
| RugCheck Score   | [Computed]          | `<score> (cap 400)` | [SAFE / REJECT]   |
| Risk Severities  | [All warn / Other]  | `<levels>`          | [SAFE / REJECT]   |
| Dev Screen       | [Pass / Reject]     | `<age>, <priors>`   | [SAFE / REJECT]   |

**Decision:** `[PROCEED_TO_SNIPE | REJECT_TOKEN]`
**Reason:** `<brief_justification>`
