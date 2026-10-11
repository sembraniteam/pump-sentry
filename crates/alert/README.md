# alert

Non-blocking Discord trade alerts via Hermes CLI (skill
`discord-hermes-alerting`).

## Architecture

```
Alerter::send(card)
  └─ tokio::spawn detached: `hermes send --to discord "<card>"`
     ├─ 5s timeout → warn only
     ├─ spawn failure → warn only
     ├─ non-zero exit → warn with stderr
     └─ success → silent
```

Never blocks pricing or swap paths; never fails the caller.

- `discord::Alerter` — `{binary, enabled, timeout}`; `disabled()` is a
  no-op for tests and silent runs; `with_binary()` redirects dispatch
  (tests use `/bin/echo` and `/bin/false`).
- Cards — skill formats verbatim: `format_entry` (🟢 open with TP/SL),
  `format_exit` (🎯/🛑 with reason, signed PnL, equity, moonbag tag),
  plus `format_live_buy` / `format_live_sell` carrying the Jito bundle
  UUID for tracking.

## Use

```rust
use alert::Alerter;

let alerts = Alerter::new(); // enabled, shells out to `hermes`
alerts.send(Alerter::format_entry("MINT", 0.0000042, 40.0, -10.0, 10.0));
```

 silence:

```rust
use alert::Alerter;

let silent = Alerter::disabled(); // or DISCORD_ALERTS=false in the bot
silent.send("dropped silently".to_string());
```

## Test

```sh
cargo test -p alert
```
