# Cryptograph Command Reference

Every interface Cryptograph exposes: Telegram commands, the inline buttons
attached to reports, and the CLI used for diagnostics and cron-style runs.

- Commands are case-insensitive and accept arguments either as ` /price BTC ETH`
  or as plain text where noted.
- Access is restricted to the IDs in `ALLOWED_ADMIN_IDS` (plus anything added at
  runtime with `/allow`). Non-admin requests are rejected before any handler
  runs.
- `/forecast` is always rendered in English. `/dip`, `/sniper` and `/bottom`
  follow the user's `preferred_language` setting (`ru` → Russian, otherwise
  English).

---

## 1. Conversation

| Command | Description |
|---|---|
| `/start`, `/b` | Greeting and the main inline menu |
| `/help` | Command reference |
| `/cancel` | Abort the current FSM wizard (add purchase, add alert, add wallet) |

**Plain text is a question.** Anything that is not a command is routed to the
`/ask` pipeline, so you never have to type `/ask`.

**Reply context.** Replying to *any* message — a bot report, an alert card or
your own earlier message — injects that message (tagged with its author) into
the AI context. This is how you continue a thread: send a forecast, reply to it
with "а если подождать до декабря?", and the model sees the original table
plus the last six exchanges of the conversation (persisted in SQLite, so it
survives restarts).

---

## 2. Market data

| Command | Description |
|---|---|
| `/price` | Quotes for your watchlist |
| `/price BTC ETH SOL` | Quotes for explicit symbols |
| `/top` | Top 10 coins by 24h volume |
| `/top 25` | Top N coins |
| `/convert 100 BYN BTC` | Convert between fiat and coins |
| `/fng` | Fear & Greed Index |
| `/news` | General market news digest |
| `/news BTC` | Symbol-specific AI summary |

Prices come from the consensus engine (16 providers, median with outlier
rejection, 180 s cache). No command here consumes an AI quota except `/news`,
which reads a pre-computed summary when it is fresh.

---

## 3. Portfolio

| Command | Description |
|---|---|
| `/portfolio` | Holdings, current value, cost basis, P&L |
| `/pnl` | P&L timeline chart (PNG) |
| `/distribution` | Allocation donut (PNG) |
| `/add BTC 0.01 600 hot:binance` | Record a purchase (`SYM AMOUNT COST [wallet]`) |
| `/move BTC 0.5 from:ledger to:binance fee:0.0001` | Transfer between wallets |
| `/adjust wallet:ledger BTC 0.9999` | Set a holding balance manually |
| `/correct #5 amount 0.02` | Edit a recorded purchase by ID |
| `/invested BTC 5000` | Override the total invested amount for a symbol |
| `/history` | Every purchase, plus per-asset amount / average entry / total cost |
| `/history BTC` | The same report filtered to one symbol |
| `/watchlist add ETH` | Add to the default watchlist |
| `/watchlist remove ETH` | Remove from the watchlist |
| `/watchlist list` | Show the watchlist |
| `/wallet add cold:ledger BTC bc1q...` | Register a wallet (on-chain tracking when an address is given) |
| `/wallet list` | Wallets and balances |
| `/wallet refresh` | Force an on-chain balance refresh |
| `/backup` | Send a versioned JSON backup |
| `/restore` | Restore from a JSON backup (send the file after the command) |

---

## 4. Alerts

| Command | Description |
|---|---|
| `/alert list` | All configured rules |
| `/alert add BTC < 60000 cooldown:6h` | Absolute price threshold |
| `/alert add ETH > 4000` | Absolute price threshold (above) |
| `/alert add BTC 24h < -5%` | Percentage move over a period (`1h`, `4h`, `24h`, `7d`, `30d`, `90d`) |
| `/alert del 1` | Delete a rule |
| `/alert reset` | Remove all rules |

Rules are evaluated on a 60 s tick against cached quotes. A rule must hold for
two consecutive ticks before it fires (wick protection), and each rule has its
own cooldown. Quiet hours silence low-severity alerts.

The **Add Alert** button starts a wizard instead of requiring the syntax above.

---

## 5. Analysis

| Command | Description |
|---|---|
| `/ask <question>` | Portfolio-aware AI answer (plain text works too) |
| `/ai` | Deep analysis of your holdings |
| `/ai BTC` | Deep analysis of one symbol |
| `/ai set BTC ETH SOL` | Set the multi-asset analysis list |
| `/advice` | Long-term investment intelligence and market sentiment |
| `/patch <text>` | Fix portfolio records from natural language |
| `/path <text>` | Alias of `/patch` |

`/ask` is answered by two racing generators, merged and fact-checked by a
critic against the live snapshot. If every free provider is exhausted the
answer is still produced deterministically from the portfolio, local history
and the forecast model.

---

## 6. Deterministic models

These commands do not call an AI provider and do not consume quota. Output is
reproducible for the same inputs.

### `/forecast [SYMS]`

A horizon ladder over the local daily-close store (1/3/5/7/14/30/90/180 days;
shortened for younger assets), each horizon with a median, a Student-t
68% cone and the probability of finishing above spot. The bottom block compares
spot to your average entry, gives the probability of reaching the dip band and
your break-even, and states an explicit action.

| Form | Description |
|---|---|
| `/forecast` | Your watchlist |
| `/forecast BTC ETH` | Explicit symbols |
| `/forecast verify` | Walk-forward backtest of the shipped model |
| `/forecast verify BTC` | Backtest for one symbol |

The report also prints the model's real out-of-sample direction hit rate, MAPE
and cone coverage per horizon. `SIDEWAYS` regimes have momentum zeroed.

### `/dip [SYMS]`

Support levels below spot from SMA50/200, recent lows and the forecast cone,
each priced with a first-passage probability of being touched within
1/3/7/14/30 days. Verdict is `BUY`, `SPLIT`, `WAIT` or `NO-BUY` relative to your
average entry.

### `/sniper [SYMS]`

Tail-risk accumulation ladder. Settings:

| Form | Description |
|---|---|
| `/sniper BTC` | Report for a symbol |
| `/sniper cash 1000` | Cash pool waiting for a dip |
| `/sniper protect on\|off` | Average-entry guard (flags buckets that raise your cost basis) |
| `/sniper alerts on\|off` | Proactive dip alerts |

Report contents: 14-day drawdown Z-score, three left-tail limit buckets
(12% / 4% / 1% Student-t quantiles) with Martingale cash weights, touch
probabilities, the resulting average entry, and a 0–100 capitulation score.

### `/bottom [SYMS]`

The explicit answer to "is this the bottom, should I buy": how deep the current
move is, whether a deeper bucket is still statistically likely, and the price
at which buying makes sense. Works for symbols you do not hold yet.

### Report buttons

Every deterministic report carries inline actions:

| Button | Callback | Effect |
|---|---|---|
| 🧠 AI Analysis | `fc:ai`, `dip:ai`, `snp:ai`, `btm:ai` | AI narrates the deterministic report; the numbers are passed in and cannot be changed |
| 🔄 Refresh | `fc:fx`, `dip:fx`, `snp:fx`, `btm:fx` | Rebuild from the freshest data in place |
| 🪙 Dip zones | `dip:fx:SYM` | Jump to the dip report |
| 🩸 Sniper | `snp:cmd:SYM` | Jump to the sniper report |
| 🧭 Bottom | `btm:fx:SYM` | Jump to the bottom radar |
| 📈 Forecast | `fc:fx:SYM` | Jump to the forecast |

The AI buttons use a single-pass fast path (no racing, no critic) so they answer
in seconds instead of half a minute.

---

## 7. Proactive memory

| Command | Description |
|---|---|
| `/memo invest 100 USD at a time` | Permanent rule the AI must respect |
| `/memo list` | All memos |
| `/memo del 2` | Delete a memo |
| `/goal want to spend 400 USD this month` | Standing goal; the AI reports when conditions fit |
| `/goal list` | Goals with status and progress |
| `/goal progress 3 bought 150 of 400` | Append progress |
| `/goal done 3` | Mark complete |
| `/goal resume 3` | Reactivate |
| `/goal del 3` | Delete |
| `/plan` | Dashboard over goals, memos and alerts |
| `/plan status` | Current plan state |
| `/plan check` | Ask the AI to evaluate the plan now |
| `/plan ask <question>` | Ask about the plan |

Goals cannot be silently deactivated by the model: it proposes, and you confirm.

---

## 8. Trading module

The trading subsystem defaults to simulation.

| Command | Description |
|---|---|
| `/trade list` | Allocations, constraints, realized and unrealized P&L |
| `/trade pnl` | P&L summary |
| `/trade allocate SOL 50` | Create a capital pool |
| `/trade rule add BTC ceiling 65000` | Operator constraint (`ceiling`, `floor`, `drawdown`, `pause-buy`, `pause-sell`) |
| `/trade rule del ...` | Remove a constraint |
| `/trade fee <buy> <sell>` | Per-mode fee configuration |
| `/tradetask list` | Standing advisor tasks |
| `/tradetask add --name N --assets BTC,ETH` | Create a task |
| `/tradetask pause` / `/tradetask resume` | Suspend or resume a task |
| `/tradetask advise` | Run the advisor now |

The full engine (backtests, strategy training, commander mode, DEX quotes) is
available from the CLI — see [trading/README.md](trading/README.md).

---

## 9. System

| Command | Description |
|---|---|
| `/settings` | Interactive settings: watchlist, auto-backup, currency display, quiet hours, AI language, statistics, model health, source reliability |
| `/model list` | Ranked models and roles |
| `/model set 3` | Pin a preferred model |
| `/ver` | Build version and git hash |
| `/reset` | Clear caches, statistics and model health (portfolio is kept) |
| `/allow 123456789` | Grant access |
| `/revoke 123456789` | Revoke access |
| `/acl` | List approved users |

There is no separate `/stats`: statistics live inside `/settings`.

---

## 10. CLI

The binary doubles as a diagnostic tool. This is how the bot is verified in
production without touching Telegram.

```bash
cryptograph version
cryptograph analyze BTC
cryptograph forecast BTC ETH
cryptograph forecast verify BTC
cryptograph backtest BTC ETH SOL LINK
cryptograph dip BTC
cryptograph sniper BTC cash=1000 avg=64255 pos=0.5 protect=on
cryptograph bottom BTC cash=1000 avg=64255 pos=0.5
cryptograph ask demo "should I buy more?"
cryptograph ask 123456789 "what changed?" reply="[assistant (bot)]: BTC 30d ..."
cryptograph goal demo -v
cryptograph scenario scenarios/euphoria.json "buy now?" --fast
cryptograph search "ETH ETF flows"
cryptograph news summary BTC
cryptograph news history BTC 10
cryptograph news articles BTC
cryptograph news fetch BTC
cryptograph news cleanup
cryptograph news run-once
cryptograph models refresh
cryptograph models list
cryptograph models rank
cryptograph models stats 7
cryptograph models calibrate
cryptograph trade status
cryptograph wipe-knowledge
cryptograph --commentary BTC
```

| Subcommand | Purpose |
|---|---|
| `version` | Same identity `/ver` reports |
| `analyze <SYM>` | Deep AI analysis in the terminal |
| `forecast <SYMS>` | Horizon ladder (add `verify` for the walk-forward backtest) |
| `backtest <SYMS>` | Shipped vs calibrated model over a long and a short window |
| `dip <SYMS>` | Dip ladder and verdict |
| `sniper <SYMS> cash= avg= pos= protect=` | Tail-risk ladder |
| `bottom <SYMS> cash= avg= pos=` | Bottom radar |
| `ask <user_id\|demo> "q" [reply="..."]` | Full `/ask` pipeline with every stage printed |
| `goal <user_id\|demo> [-v]` | One proactive scan, stage by stage |
| `scenario <file.json> ["q"] [--fast]` | Deterministic what-if against a frozen market |
| `search "query"` | Only the web-search planner and results |
| `news summary\|history\|articles\|fetch\|cleanup\|run-once` | Inspect and sweep the news store |
| `models refresh\|list\|rank\|stats\|calibrate` | Model registry and benchmarking |
| `trade ...` | Full trading module CLI |
| `wipe-knowledge` | Reset caches, keep users, goals, memos and budgets |
| `--commentary <SYMS>` | Exercise the exact path behind the forecast AI button |

Set `CRYPTOGRAPH_FAST_MODELS=1` to force the cheap watcher-tier models in
`scenario` runs.
