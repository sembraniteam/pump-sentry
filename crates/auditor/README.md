# auditor — RugCheck token-risk client

Rejects rugs/honeypots before buying: `evaluate_token` (policy-enforced; raw fetch is private so callers can't bypass policy).

- Policy: score cap 400, no `danger`, no mint/freeze authority; missing data rejects (fail-closed).
- Retries (1500ms timeout, backoff 200ms×2^n ≤2s): timeouts, connects, body stalls, 5xx, 429, 408. Fresh-mint 400s are NOT retried here — the pipeline watchlists and retries every 3s. Oversize bodies and schema breaks are terminal, never retried.

```rust
let report = RugCheckClient::new()?.evaluate_token(mint).await?;
```

```sh
cargo test -p auditor
```
