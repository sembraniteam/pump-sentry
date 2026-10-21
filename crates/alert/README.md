# alert — Discord trade cards over HTTPS

Fire-and-forget, never blocks pricing or swaps, never fails the caller (5s timeout, warn-only).
Delivery is a direct `POST /channels/{id}/messages` with `Authorization: Bot …` —
no subprocess, no external agent.

- At most 4 in flight; storms drop with a counter. Sends need both a bot token
  (`DISCORD_BOT_TOKEN` env or `DISCORD_BOT_TOKEN_FILE`, captured at boot and
  scrubbed after) and a numeric channel id; without either, sends no-op.
- Cards: entry/exit (signed PnL, moonbag tag), live buy/sell (bundle UUID), migration, 2x follow-spike, 6h heartbeat. Fields sanitized and length-capped (content truncated at 2000 chars, never split).
- The heartbeat carries the health view: RPC pool (`n/m ok · failovers`, inconsistency flag), data-degrade state, oldest open age, stream gaps shed (slots + capped backfills), and windowed GiveUps (buy/sell failures).

```sh
cargo test -p alert
```
