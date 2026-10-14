# alert — Discord trade cards via Hermes CLI

Fire-and-forget, never blocks pricing or swaps, never fails the caller (5s timeout, warn-only).

- At most 4 in flight; storms drop with a counter. `HERMES_BIN` overrides the binary (absolute preferred).
- Cards: entry/exit (signed PnL, moonbag tag), live buy/sell (bundle UUID), migration, 2x follow-spike, 6h heartbeat. Fields sanitized and length-capped.

```rust
Alerter::new().send(Alerter::format_entry("MINT", 0.0000042, 40.0, -10.0, 10.0));
```

```sh
cargo test -p alert
```
