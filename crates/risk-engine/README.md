# risk-engine — LLM TP/SL with deterministic fallback

Model decides within 300ms (floor 50ms, cap 2s); anything unusable falls back to the heuristic. `evaluate` never fails on model trouble.

- Shapes over a BUY profile: `adaptive` verbatim; `scalp` clamps TP into +25…+35% (sub-band honored, never widened); `runner` forces moonbag + trailing (a trail is forced onto any trailless moonbag).
- Heuristic: PASS on `dev < 0.40`, `rugcheck > 350`, or velocity < 5; momentum tiers map to fixed TP/SL/timeout.

```sh
cargo test -p risk-engine
```
