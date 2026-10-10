# auditor

Async [RugCheck.xyz](https://rugcheck.xyz) token-risk client used by the
snipe pipeline to reject rugs / honeypots before buying.

## Architecture

```
fetch_report  →  GET {base}/tokens/{mint}/report (timeout + retries)
evaluate_token →  fetch_report + policy invariants → Report | SecurityRejection
```

- `types` — `RugCheckReport`, `RiskItem`, `TokenInfo`, `RugCheckError`
  (skill `solana-rugcheck-api` contract).
- `policy` — pure `assess(report, policy) -> Verdict::{Allow, Reject}`:
  score cap (default 400), no `danger` risks, no mint/freeze authority.
- `client` — pooled `reqwest` client, 1500ms timeout, retries on
  timeouts / connect errors / 5xx / 429 with exponential backoff
  (200ms × 2^attempt, max 2s).

## Use

```rust
use auditor::RugCheckClient;

let auditor = RugCheckClient::new();
match auditor.evaluate_token("7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU").await {
    Ok(report) => println!("allow, score={}", report.score),
    Err(auditor::RugCheckError::SecurityRejection(reason)) => println!("reject: {reason}"),
    Err(e) => eprintln!("audit failed: {e}"),
}
```

Custom policy / mirror endpoint:

```rust
use auditor::{AuditPolicy, RugCheckClient};
use std::time::Duration;

let auditor = RugCheckClient::builder()
    .policy(AuditPolicy { max_score: 200, ..Default::default() })
    .timeout(Duration::from_millis(800))
    .max_retries(1)
    .build();
```

## Test

```sh
cargo test -p auditor
```
