# Roadmap — next release (post-0.3.0)

Planned, not started. Do not implement without re-reading the
constraints below — the design risks are load-bearing.

## RPC endpoint fallback (REST + WebSocket)

Goal: when one endpoint degrades, shift to another (reads + WS;
never blind dispatch resends).

Why feasible here: single URL source (`config.rs` KEYMAP), an
existing WS reconnect loop with backoff + gap backfill
(`listener/src/stream.rs`), and 5 `RpcClient` construction sites
that are all read/sim except dispatch.

Constraints (from audit):

1. Failure detection is soft (429 / timeout / stale slot, rarely a
   clean disconnect). Failover needs classification + hysteresis:
   sticky primary, switch after N consecutive failures, cooldown
   before returning. Flapping burns rate limits everywhere at once.
2. Slot-lag differs per node. Carry the slot watermark across the
   switch (never reset); seen-cache dedups, fail-closed guards hold
   mispriced ticks.
3. Dispatch path must NOT fail over blind sends. Timeout != failure
   (no-resend rule — a timed-out bundle may have landed). Failover
   covers reads/sim only. Jito block-engine is already a separate
   endpoint, so this boundary is clean.
4. Backup = second paid Helius key or public node (public = 429-prone:
   explicit degraded-mode expectations, not silent parity).
5. Prove with fault injection: wrap transport in a trait, test with
   fake dying/recovering endpoints (deterministic switch, backoff,
   hysteresis). An untested failover is unproven.

Suggested order: endpoint pool + hysteresis on the read path first
(no dispatch changes), fault-injection tests, then WS resubscribe
rotation reusing the existing reconnect/backfill machinery.
