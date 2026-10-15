# simulator — paper-trading ledger

Virtual cash, positions, and exits. Proves profitability before real funds move.

- Fills: 125 bps/side + constant-product impact (25% cap); buys fill high, sells low.
  The cap binds ONLY the infallible sizing estimators (`slip_fraction*`,
  `fill_price`): at 125 bps/side it starts understating sell cost past
  ~31% of book liquidity. Booked exits and marks price uncapped true
  impact instead (whale Long exits print near zero; buy-side exhaustion
  past the curve rejects `CurveExhausted` instead of printing).
- Exits, in order: timeout → TP → SL → trailing; moonbag scale-outs ratchet to breakeven with a 24h backstop.
- No leverage (`notional + fee ≤ cash`); equity marks exits at modeled proceeds minus fees.

```sh
cargo test -p simulator
```
