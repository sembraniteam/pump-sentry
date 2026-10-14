# risk-engine

LLM-driven Take-Profit / Stop-Loss engine with a deterministic fallback.
Consumes the prompt schema from skill `adaptive-risk-prompting`.

## Architecture

```
evaluate(metrics)
  ├─ POST {endpoint} (OpenAI-compatible chat, temperature 0.1, JSON mode)
  │    └─ valid in-schema profile within 300ms → DecisionSource::Model
  └─ timeout / error / non-2xx / bad JSON / invalid profile
       └─ evaluate_heuristic_fallback → DecisionSource::Fallback
```

- `schema` — `TokenMetrics` → skill `INPUT PAYLOAD`; `RiskProfile`
  (`BUY|PASS`, `tp_percent`, `sl_percent`, `trailing_stop`,
  `timeout_seconds`, `moonbag_percent`) + `is_valid()` guard (moonbag
  must stay below 100% — a TP selling nothing could never exit).
- `strategy` — execution shapes over a BUY profile: `adaptive`
  (verbatim), `scalp` (TP cut down into +25…+35%, full exits; sub-band
  TPs pass through with a warning, never widened silently), `runner`
  (forced moonbag + trailing). PASS is never rewritten; any moonbag
  without a trail gets the trail forced on (a sideways runner would
  otherwise never exit).
- `fallback` — skill decision table verbatim: PASS when `dev_reputation
  < 0.40`, `rugcheck_score > 350`, or `velocity < 5`; high momentum (vel > 25,
  liq > $20k) → TP 60 / SL −10 / trail / 180s; moderate (vel ≥ 5, liq ≥ $10k) → TP 35 / SL −8 / 90s; otherwise PASS
  (never
  buy dead tokens).
- `engine` — `RiskEngine` (pooled HTTP client, `FALLBACK_TIMEOUT` =
  300ms). Model output is schema-validated; anything unusable resolves
  to the heuristic, so `evaluate` never fails on model trouble.

## Use

```rust
use risk_engine::{RiskEngine, TokenMetrics};

let engine = RiskEngine::builder()
.endpoint("http://127.0.0.1:11434/v1/chat/completions")
.model("llama3.2:1b")
.build() ?;

let decision = engine.evaluate( & TokenMetrics {
liquidity_usd: 30_000.0,
buy_velocity_5s: 40,
dev_reputation: 0.8,
rugcheck_score: 120,
}).await;

match decision.profile.action {
risk_engine::Action::Buy => println!("BUY tp={} sl={}", decision.profile.tp_percent, decision.profile.sl_percent),
risk_engine::Action::Pass => println!("PASS"),
}
```

## Test

```sh
cargo test -p risk-engine
```
