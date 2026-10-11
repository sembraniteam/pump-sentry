# simulator

Paper-trading simulator: virtual balances, active positions, PnL.
Lets the pipeline prove profitability before risking real funds.

## Architecture

```
PaperPortfolio::new(100.0, fee_bps)
  ├─ open_position_params(OpenPositionParams{...})   // + moonbag share
  │    └─ FillModel quotes → execution price; cash −= notional + fee
  ├─ check_exit(&mut position, price, now) → TP / SL / trailing / timeout
  ├─ close_position(id, quote, liquidity, reason) → ClosedTrade{gross,fees,net}
  │    └─ partial TP close scales out, ratchets the runner to breakeven,
  │       disarms moonbag, and lifts the time-stop (runners ride trailing)
  └─ snapshot(quotes) → {cash, locked, equity, realized, unrealized}
       (open legs valued at exit proceeds: fill-model price minus exit fee)
```

- `types` — `Position`, `ClosedTrade`, `Side::{Long,Short}`,
  `ExitReason::{TakeProfit,StopLoss,TrailingStop,Timeout,Manual}`.
- `market` — `FillModel::{Exact,BondingCurve}` cost model (default 125 bps
  venue fee per side + exact constant-product impact, 25% cap; buys fill
  high, sells fill low — ~2.9% round trip on a $10 ticket into $4500),
  `move_percent`, `check_exit` with trailing ratchet.
- `portfolio` — cash ledger (no leverage: entries need
  `notional + fee ≤ cash`), position map, trade history, `realized_pnl`,
  `win_rate`, mark-to-market `snapshot`.

## Use

```rust
use simulator::{ExitReason, FillModel, OpenPositionParams, PaperPortfolio, Side, check_exit};
use std::collections::HashMap;

let mut pf = PaperPortfolio::new(100.0, 30.0); // $100, 0.30%/side
let id = pf.open_position_params( & OpenPositionParams {
mint: "MINT", side: Side::Long, quote: 1.0, notional_usd: 40.0,
liquidity_usd: 50_000.0, tp_percent: 75.0, sl_percent: - 8.0,
trailing: true, trail_percent: 5.0, timeout_seconds: 180,
moonbag_percent: 0.0, now_secs: 1_700_000_000,
}) ?;

// Poll loop: evaluate triggers, close on fire.
let reason = check_exit(pf.position_mut(id).unwrap(), 1.80, 1_700_000_010)
.unwrap_or(ExitReason::Manual);
let trade = pf.close_position(id, 1.80, 50_000.0, reason, 1_700_000_010) ?;
println!("net {:+.2} ({}%)", trade.net_pnl, trade.net_pnl_percent);

let mut quotes = HashMap::new();
let snap = pf.snapshot( & quotes);
println!("equity={:.2} realized={:+.2}", snap.equity, snap.realized_pnl);
```

## Test

```sh
cargo test -p simulator
```
