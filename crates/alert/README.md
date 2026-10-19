# alert — Discord trade cards via Hermes CLI

Fire-and-forget, never blocks pricing or swaps, never fails the caller (5s timeout, warn-only).

- At most 4 in flight; storms drop with a counter. `HERMES_BIN` overrides the binary (absolute preferred).
- Cards: entry/exit (signed PnL, moonbag tag), live buy/sell (bundle UUID), migration, 2x follow-spike, 6h heartbeat. Fields sanitized and length-capped.
- The heartbeat carries the health view: RPC pool (`n/m ok · failovers`, inconsistency flag), data-degrade state, oldest open age, stream gaps shed (slots + capped backfills), and windowed GiveUps (buy/sell failures).

```sh
cargo test -p alert
```
