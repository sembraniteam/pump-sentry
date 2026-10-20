# Roadmap — next release (post-0.8.2)

## Shared RPC pool — DONE

One `Arc<RpcClient>` built in `main` and injected via `with_rpc`
constructors (`CurveTicker`, `VaultTicker`, `RpcScreener`,
`SwapExecutor`, `PumpFunListener::subscribe_with_rpc`). Retry/backoff
is now coordinated instead of five uncoordinated pools hammering the
rate limit from different corners.

## Yellowstone gRPC ingestion — LIVE-PROVEN (Oct-2026)

`listener/src/geyser.rs` (`GeyserSubscriber`): selected automatically
when `[network].yellowstone_endpoint` is non-empty, WS otherwise —
never both. Full transactions arrive inline (no per-candidate
`getTransaction` fetch, no saturation drops); gap coverage rides a
bounded REST `getBlock` backfill per reconnect (`from_slot` replay is
server-rejected below `processed`, so it is only sent there).
Unit-proven against the frozen `create_v2` fixture (same name/symbol
assertions as the WS path); live-proven against PublicNode (subscribe
line, flowing launches, `parse_ok` 1:1, zero drops).

## RPC endpoint fallback (REST + WebSocket)

Goal: when one endpoint degrades, shift to another (reads + WS;
never blind dispatch resends).

Why feasible here: single URL source (`config.rs` KEYMAP), an
existing WS reconnect loop with backoff + gap backfill
(`listener/src/stream.rs`), and one shared `RpcClient` for all
read/sim paths (dispatch excluded by construction).

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
