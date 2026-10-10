# simulator

Paper-trading simulator: virtual balances, active positions, PnL.
Lets the pipeline prove profitability before risking real funds.

## Architecture

```
PaperPortfolio::new(100.0, fee_bps)
  ├─ open_position(mint, side, quote, notional, liquidity, tp/sl/trail/timeout)
  │    └─ FillModel quotes → execution price; cash −= notional + fee
  ├─ check_exit(&mut position, price, now) → TP / SL / trailing / timeout
  ├─ close_position(id, quote, liquidity, reason) → ClosedTrade{gross,fees,net}
  └─ snapshot(quotes) → {cash, locked, equity, realized, unrealized}
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
use simulator::{ExitReason, FillModel, PaperPortfolio, Side, check_exit};
use std::collections::HashMap;

let mut pf = PaperPortfolio::new(100.0, 30.0); // $100, 0.30%/side
let id = pf.open_position("MINT", Side::Long, 1.0, 40.0, 50_000.0,
75.0, - 8.0, true, 5.0, 180, 1_700_000_000) ?;

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
