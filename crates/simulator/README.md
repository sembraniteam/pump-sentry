# simulator — paper-trading ledger

Virtual cash, positions, and exits. Proves profitability before real funds move.

- Fills: 125 bps/side + constant-product impact. Booked exits and
  marks price uncapped true impact (whale Long exits print near zero;
  buy-side exhaustion past the curve rejects `CurveExhausted`).
  The 25% cap binds ONLY the infallible sizing estimators
  (`slip_fraction*`, `fill_price`), never a booked fill.
- Sizes are whole tokens (`u64`, direction from `side`): sub-token dust
  cannot open and zero-size partials reject, matching live dust
  handling. Cash, fees, and percents key off the actually-invested
  size (`qty × entry`), never the nominal ticket.
- Paper dispatch bleed mirrors live debiting per leg (tip + priority +
  base fee; ATA rent on open) — a calm-weather floor, not a ceiling:
  live pays escalated tips under congestion.
- Exits, in order: timeout → TP → SL → trailing; moonbag scale-outs ratchet to breakeven with a 24h backstop.
- No leverage (`notional + fee ≤ cash`); equity marks exits at modeled proceeds minus fees.
- Dispatch costs can outrun cash: the ledger carries the negative cash instead of flooring
  it, `PortfolioSnapshot::overdraft_usd` names the shortfall (the heartbeat warns), and new
  Long opens refuse while cash is negative.

```sh
cargo test -p simulator
```
