# Database & Data Models Architecture

Cryptograph is **SQLite-first**: a single `crypto.db` (WAL mode) is the source
of truth for user data, caches, budgets, and model telemetry. A small in-memory
`Store` holds the hot portfolio state for speed and is persisted to SQLite as
versioned JSON on every mutation. The trading module uses its own pair of
SQLite files (`trading_sim.db` / `trading_live.db`) with a shared schema.

---

## 1. The Core Store (`src/db/store.rs`)

The `Store` is the central brain of Cryptograph. It holds user portfolios,
alerts, purchases, memos, goals, and settings in RAM while the bot is running.

### Why In-Memory?
- **Speed**: evaluating hundreds of price alerts every minute across thousands
  of assets is instant because there are no disk I/O bottlenecks.
- **Ephemeral session key**: a random 32-byte key is generated per process in
  `Config::from_env` (`src/config.rs`, `Zeroizing<[u8;32]>` — no `MASTER_KEY` env
  var anymore); it is zeroized on drop. Encryption helpers live in
  `src/security/cipher.rs`.

### Persistence
- Every mutation serializes the portfolio into versioned JSON
  (`BackupEnvelope`) and writes it into the SQLite `users` table for crash
  recovery.
- `/backup` exports the portfolio as `cryptograph_backup.json`; `/restore`
  re-imports it. Legacy `.cgrp` files are still accepted. The format is plain
  JSON — **not encrypted**, so treat backup files as sensitive.
- Memos and goals live in **dedicated tables** (`user_memos`, `user_goals`), so
  they survive even if the `users` table is ever rebuilt.

---

## 2. The Data Models (`src/models/`)

### `portfolio.rs`
The aggregate root: `user_id`, `purchases`, `wallets`, `alerts`, `transfers`,
`settings`, `memos`, `goals`.

### `purchase.rs`
Records the acquisition of an asset. `cost_usd` (cost basis) is the ground
truth; `/correct` appends a `CorrectionEntry` instead of mutating history,
preserving a perfect audit trail.

### `holding.rs`
Holdings are not stored as static balances — they are derived in real time from
`Purchases`, `Transfers`, and blockchain synchronizations.

### `wallet.rs`
Supports `Cold`, `Hot`, and `Exchange` wallets, with optional on-chain tracking
(`address`, `network`) and a `last_balance` map for delta notifications.

### `alert.rs`
Robust alert conditions: `PriceAbove` / `PriceBelow` / `DropPercent` /
`RisePercent`, with a `cooldown` mechanism to prevent spam.

---

## 3. SQLite Schema (`src/db/sqlite.rs`)

The schema self-migrates on every boot: `CREATE TABLE IF NOT EXISTS` for new
tables and guarded `ALTER TABLE ... ADD COLUMN` for new columns. WAL mode gives
fast concurrent reads.

### Core tables

| Table | Purpose |
|---|---|
| `users` | `id`, `portfolio_data` (versioned JSON backup), `last_updated` |
| `events` | Audit log of user-triggered events (7-day retention) |
| `price_history` | One OHLC close row per symbol per UTC day — the local candle store behind `/forecast`, `/dip`, `/sniper`, `/bottom` and the AI trajectory memory (400-day retention) |
| `news_summaries` | Latest AI-verified news digest per symbol (markdown/emoji stripped on write) |
| `news_articles` | Raw articles per symbol, dedup by URL, 3-day retention (on-demand "extended results") |
| `news_summary_history` | Append-only per-symbol summary history, 14-day retention ("what changed over time") |
| `quote_meta_cache` | Non-real-time token metadata (rank, supply, 1h/24h/7d/30d/90d changes) |
| `model_health` | Provider health status (up/down, error, last checked) |
| `macro_cache` | Latest macro context summary |
| `price_milestones` | Recorded ATH / milestone prices per symbol |
| `user_memos` | Per-user memos (hard rules the AI must respect) |
| `user_goals` | Per-user goals with progress, verdicts, and cooldowns |
| `proactive_cache` | Last proactive scan snapshot + analysis per user |
| `ask_history` | Recent `/ask` questions and answers per user (used as conversational memory) |

### Budget & usage tables

| Table | Purpose |
|---|---|
| `monthly_usage` | Monthly token/request totals per category (DeepSeek budget) |
| `paid_usage` | Per-model daily paid usage (DeepSeek) |
| `daily_usage` | Per-category daily totals (OpenRouter 300/day budget) |
| `ai_stats` | Legacy per-model ok/err counters |
| `daily_metrics` | Per-category daily ok/err counters (31-day retention) |

### Model ranking tables

| Table | Purpose |
|---|---|
| `model_registry` | Discovered models: provider, model_id, alias, display, context_length, fingerprint (OpenRouter `updated`), first/last seen, last tested, tested fingerprint, bench count |
| `model_stats` | Per-model daily calls, errors, total latency, quality score, and `bench_calls` (benchmark-only counter so production traffic cannot dilute judge quality) |

`model_registry` is fed by `models refresh` (models.json + OpenRouter free
models) and consumed by the lazy ranker; `model_stats` drives
underperformance detection and role assignment.

### Decision journal (`journal`)

A single bounded audit table of everything the bot did, so past behaviour can
be analysed without grepping live logs.

| Column | Meaning |
|---|---|
| `kind` | `inbound` (every update seen by the ACL filter), `decision` (`/decide`, `/ladder`, `/dip`, `/sniper`, `/forecast`, `/bottom`, `/analyze`), `answer` (the reply the user actually saw) |
| `command`, `args` | Which command arrived and with what arguments (`/ladder BTC 300` → `/ladder` + `BTC 300`); callbacks store their payload |
| `symbol`, `action`, `confidence` | Per-coin verdict (`buy`/`scale`/`wait`/`hold`) |
| `reason` | One-line human reason behind the verdict |
| `context_json` | Market snapshot at decision time: spot, avg entry, RSI, σ, regime, solved levels with `P(touch)` |
| `result` | Truncated text of the answer |
| `outcome` | Reserved for later grading |

It is a **ring buffer**: after each insert everything older than the newest
`JOURNAL_KEEP_LAST` (1000) rows is deleted, every text field is clipped to 1200
characters, and rows older than `JOURNAL_MAX_AGE_DAYS` (120) are swept at
startup. Steady-state size is 1–3 MB regardless of uptime. WAL is capped at
1 MB via `journal_size_limit`.

Inspect it with `docker exec cryptograph ./cryptograph journal [N] [kind]`,
`journal stats`, `journal grade 14` (score past decisions against the price
actually realised 14 days later, from local `price_history`), `journal prune`,
or `journal selftest` (write-and-read-back check). The same data is available
in Telegram via `/journal`.

### Outbound message log (`outbound_messages`)

The exact text the bot **sent**, so a user report ("it sent me garbage") can be
checked against reality instead of reconstructed from news rows or guessed.
Telegram gives a bot no way to read its own outgoing history, so the send paths
record the message themselves through `crate::bot::outbound::record`.

| Column | Meaning |
|---|---|
| `user_id` | Recipient chat id |
| `kind` | Where it came from: `rich` (rich-message payload), `ai` (HTML AI reply), `security` / `security_failed`, `watcher` / `watcher_failed` (proactive alerts), `dip_alert`, `price_alert`, `selftest` |
| `text` | The delivered text, clipped to 4000 characters |

Written by the proactive/AI send paths only — proactive messages are the ones a
user cannot re-trigger on demand, which is exactly when the text matters. It is
a ring buffer: newest `OUTBOUND_KEEP_LAST` (300) rows, `OUTBOUND_MAX_AGE_DAYS`
(14) days, per-row cap 4000 chars. A one-line preview is also logged at `INFO`
so `docker logs` shows recent sends without querying the DB.

Inspect with `docker exec cryptograph ./cryptograph outbound [N]` (newest
first) or `outbound selftest` (write-and-read-back check).

---

## 4. Trading Module Schema (`src/trading/store/`)

The trading module keeps a **separate SQLite file per engine mode**
(`trading_sim.db` for simulation, `trading_live.db` for live) with an identical
schema, so paper trading is a 1:1 rehearsal of live behavior.

| Table | Purpose |
|---|---|
| `operator_constraints` | NLP/CLI-set limits: `MAX_PRICE_CEILING`, `MIN_PRICE_FLOOR`, `MAX_DRAWDOWN_STEP`, `PAUSE_BUY`, `PAUSE_SELL` (per mode + asset, with expiry) |
| `strategy_allocations` | Capital pools: asset, total/deployed/available budget, profit reserve, cumulative profit, status (ACTIVE/PAUSED/COMPLETED) |
| `trade_lots` | Specific-lot accounting: entry price, amount, fee paid, cost basis, target exit price, status (OPEN / COLD_HOLD / CLOSED) |
| `agent_debates` | Bull vs Bear vs Arbiter audit trail per lot (market context, rationales, verdict, action BUY/PASS/DEFER) |
| `trade_executions` | Every executed fill: side, price, amount, fee, net USD |
| `backtest_runs` | Recorded backtests: symbol, window, capital, final capital, trades, wins/losses, PnL, max drawdown |
| `strategy_lessons` | Lessons the AI learned from closed trades / backtests |
| `engine_configs` | Versioned engine configs (min profit margin, fee rate, tranche count, spread cap, crash guard, ATH guard, profit mode) with rationale + source |
| `strategy_profiles` | Strategy catalog: builtin / trained / manual profiles with engine config + default constraints + score |
| `training_stats` | Per-config training results from `strategy train` sweeps (score, avg PnL, max DD, win rate, profitable-window ratio, cold-lot ratio) — injected into AI context as the trained-strategy block |
| `trade_recommendations` | Commander-mode advice lifecycle: PENDING → EXECUTED / REJECTED / EXPIRED, with guardrails re-checked at the actual fill price |
| `trade_tasks` | Standing trade tasks (advisor mode): monthly budget, alerts/day cap, tranches, margin, execution mode |
| `exchange_fees` | Per-mode exchange fee config (buy/sell %) used by the no-loss invariant and advisor sizing |
| `regime_profiles` | Per-symbol, per-regime winning engine config learned from history, consumed by `trade regime` and `strategy regimes` |
| `seasonality_stats` | Per-symbol calendar-month average return + sample count, used as training/analysis context |
| `experiments` | Recorded engine-config comparisons over rolling windows: score, avg PnL, drawdown, win rate, profitable ratio, cold-lot ratio, windows beating buy-and-hold |
| `task_sim_runs` | Recorded `trade task sim|grid` replays: window, budget, fees, trades, realized/unrealized PnL, win rate, score |
| `fake_trades` | Synthetic fills written by task simulation and scenario replays; kept separate from `trade_executions` so they never pollute the real ledger |

### Key invariants (enforced in `src/trading/engine.rs`)
- **No-loss exit**: a lot may only be sold when
  `amount * exit * (1 - fee_rate) >= cost_basis * (1 + min_profit_margin)`.
  Underwater lots transition to `COLD_HOLD` and are never liquidated at a loss.
- **ATH guard**: a buy is skipped when the price is within
  `max_buy_from_high_pct` of the highest close over `ath_lookback_days` — cash
  stays parked instead of chasing tops.
- **Capital boundary**: allocations cannot exceed their dedicated budget pool.
- **Profit routing**: `REINVEST` / `SEPARATE` / `PARTIAL(f)` decide where
  realized profit goes; `total_profit_usd` is tracked cumulatively.

### Seed DB
`data/trading_sim_seed.db` is a bundled, read-only-at-runtime artifact: on first
run `TradingStore::open` copies it into place (pre-loading trained profiles,
lessons, and stats), so the bot never starts from zero. Refresh it after
retraining with `trade seed`. It ships as a data file next to the binary — not
`include_bytes!` — so it can be updated without rebuilding.

---

## 5. Why SQLite?
- Lightweight, no separate server, perfectly suited for a self-hosted bot.
- If cache tables are deleted, the bot rebuilds them from external APIs; user
  data (users, goals, memos) is never lost because it is restored on boot.
- `PRAGMA integrity_check` on startup; on corruption the file is renamed to
  `crypto.db.corrupt.<timestamp>` and a fresh DB is booted to avoid downtime.