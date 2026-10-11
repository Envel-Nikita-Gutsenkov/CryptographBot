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