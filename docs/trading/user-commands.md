# User Commands & Telegram Interface

> [!NOTE]
> **Status: implemented.** The Telegram `/trade` mirror (list/pnl/fee/
> allocate/deallocate/constraints/rule) and `/tradetask` mirror (list/add/
> pause/resume/del/advise) are wired in `src/bot/handlers/trade.rs`
> (admin-gated, sim DB by default, `--live` flag). The CLI remains the 1:1
> testing surface (`cargo run -- trade ...`, see `TRADING_MODULE_PLAN.md`).
> Every command below maps 1:1 to a working CLI command.

The trading system is controlled through Telegram commands. All actions are
available exclusively to admin users (enforced by `ALLOWED_ADMIN_IDS`).

---

## Modes

Every `/trade` and `/tradetask` command operates on one of two databases:

| Mode | DB | Purpose |
|---|---|---|
| `sim` (default) | `trading_sim.db` | Paper trading, training, what-if |
| `live` | `trading_live.db` | Real allocations (only when you opt in) |

Append `--live` to any command to target the live DB, e.g.
`/trade allocate BTC 100 --live`.

---

## `/forecast` — Deterministic Horizon Ladder

| Command | Description |
|---|---|
| `/forecast SYMBOL` | Point target + ~68% cone + P(above spot) per horizon, with walk-forward hit rate / MAPE / cone coverage, plus a portfolio-aware bottom line |
| `/forecast verify [SYMBOLS]` | Walk-forward backtest on realized history: MAPE / direction hit rate / cone coverage, shipped vs calibrated model |

Ladder: `1/3/5/7/14/30/90/180d` for BTC/ETH, `…60/90d` for SOL/LINK, `…30d`
for anything else; horizons are also trimmed when the local history is short
(never beyond a quarter of the observed record). Buttons: **AI Analysis**
(narrative only — the numbers stay deterministic), **Refresh** and
**Dip zones**. The bottom line compares spot to your average entry: below it
it says to scale in, above it never suggests averaging up, and it reports the
chance of the dip band / your break-even actually being reached.

```
/forecast BTC
/forecast verify BTC ETH
```

## `/dip` — Dip Zones & Buy Verdict

| Command | Description |
|---|---|
| `/dip SYMBOL` | Zones below spot with touch probabilities + `BUY` / `SPLIT` / `WAIT` / `NO-BUY` |

Zones come from SMA50/200, recent lows and the forecast cone; each carries
`P(touch within 1/3/7/14/30d)` and `P(close below at horizon)`. The verdict
also uses your average entry price, so the same report is different for two
users with different cost bases. Buttons: **AI Analysis** and **Refresh**.

```
/dip SOL
```

---

## `/trade` — Trading Control

| Command | Description |
|---|---|
| `/trade list` | Pending recommendations (advisor output) |
| `/trade pnl` | Realized + unrealized PnL across allocations |
| `/trade fee [--buy P] [--sell P]` | Show or set exchange fees (default 0.1%) |
| `/trade allocate SYMBOL USD` | Create an allocation pool (e.g. `/trade allocate BTC 100`) |
| `/trade deallocate ID` | Delete an allocation (undo) |
| `/trade constraints` | Active guardrails (price ceilings/floors, drawdown, pause rules) |
| `/trade rule SYMBOL ceiling\|floor\|drawdown VALUE` | Add a guardrail |
| `/trade rule del ID` | Delete a guardrail (undo) |

### Examples

```
/trade allocate BTC 100
Allocation created [SIMULATION]: BTC pool of 100.00 USD (id: ...)

/trade deallocate 1788773948750-4596ecea47389054
Allocation deleted [SIMULATION].

/trade rule BTC ceiling 90000
/trade rule del <id>
```

---

## `/tradetask` — Standing Advisor Tasks

Standing instructions the advisor runs periodically. This is the
"recommendation mode": the bot tells you *what* and *when* to trade, you
execute manually.

| Command | Description |
|---|---|
| `/tradetask list` | Active/paused tasks |
| `/tradetask add --name NAME [--assets BTC,ETH] [--budget 200] [--tranches 4] [--margin 0.03] [--alerts 1]` | Create a task |
| `/tradetask pause NAME` | Pause a task |
| `/tradetask resume NAME` | Resume a task |
| `/tradetask del NAME\|ID` | Delete a task (undo) |
| `/tradetask advise` | Run the advisor now for all active tasks |

### Example

```
/tradetask add --name "DCA BTC"
Task created [SIMULATION]: "DCA (id: ...)
  assets: BTC,ETH,SOL,LINK | budget $200/mo | 4 tranches | 3.0% margin

/tradetask advise
Advisor run [SIMULATION]:
• BTC — no action (price 79489.7000)
• ETH — no action (price 2494.5500)
```

When the advisor decides a buy/sell is warranted it emits a
recommendation, visible via `/trade list`. The engine never executes on its
own in recommendation mode — you confirm and report back the fill.

---

## Undo / Reversal

| Action | Command |
|---|---|
| Delete an allocation | `/trade deallocate ID` |
| Delete a guardrail | `/trade rule del ID` |
| Delete a task | `/tradetask del NAME\|ID` |

---

## DEX Execution

For on-chain execution (Jupiter on Solana, 0x/Uniswap on Ethereum) see
[dex-execution.md](dex-execution.md). The bot builds quotes and swap
payloads, the operator signs in their wallet UI; the bot never holds the
private key.

---

## Permissions

All trading commands are guarded by the admin middleware. Only users in
`ALLOWED_ADMIN_IDS` can access trading functionality.