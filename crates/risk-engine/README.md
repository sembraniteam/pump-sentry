# risk-engine — LLM TP/SL with deterministic fallback

Model decides within 300ms (floor 50ms, cap 2s); anything unusable falls back to the heuristic. `evaluate` never fails on model trouble. Undecodable or invalid model output holds PASS (only an explicit valid BUY buys); transport/timeout falls back (entries must not halt when the model is down).

- Shapes over a BUY profile: `adaptive` verbatim; `scalp` clamps TP into +25…+35% (sub-band honored, never widened); `runner` forces moonbag + trailing (a trail is forced onto any trailless moonbag).
- Heuristic: PASS on `dev < 0.40`, `rugcheck > 350`, or velocity < 5; momentum tiers map to fixed TP/SL/timeout. High-velocity flow on wash-suspect forensics (bundled creation slot or monopoly-scale cluster) drops to the moderate band, never high-momentum TP.
- Fixed decoding (`temperature 0.1`, `max_tokens 128`): deterministic numeric output; retuning voids the compliance assumptions above.
- Optional extra headers (`.header()` / `.headers()`, e.g. `x-opencode-session` for OpenCode Go routing): sent on every request; `authorization`/`user-agent`/framing headers are refused at build; values never logged.

```sh
cargo test -p risk-engine
```
