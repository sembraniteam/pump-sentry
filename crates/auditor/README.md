# auditor — RugCheck token-risk client

Rejects rugs/honeypots before buying: `fetch_report` (raw) + `evaluate_token` (policy-enforced).

- Policy: score cap 400, no `danger`, no mint/freeze authority; missing data rejects (fail-closed).
- Retries (1500ms timeout, backoff 200ms×2^n ≤2s): timeouts, connects, 5xx, 429. Fresh-mint 400s are NOT retried here — the pipeline watchlists and retries every 3s. Oversize bodies are terminal, never retried.

```rust
let report = RugCheckClient::new()?.evaluate_token(mint).await?;
```

```sh
cargo test -p auditor
```
