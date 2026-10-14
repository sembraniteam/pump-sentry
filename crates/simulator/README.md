# simulator — paper-trading ledger

Virtual cash, positions, and exits. Proves profitability before real funds move.

- Fills: 125 bps/side + constant-product impact (25% cap); buys fill high, sells low.
- Exits, in order: timeout → TP → SL → trailing; moonbag scale-outs ratchet to breakeven with a 24h backstop.
- No leverage (`notional + fee ≤ cash`); equity marks exits at modeled proceeds minus fees.

```sh
cargo test -p simulator
```
