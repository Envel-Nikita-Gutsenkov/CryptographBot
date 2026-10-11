# Autonomous Pluggable Trading & Accumulation Subsystem — Engineering Plan

> Status: **Implemented — CLI engine (sim + live), operator constraints, no-loss
> invariant, fee accounting, profit routing, backtesting (incl. batch sweep over
> 2020–2026 history), strategy catalog, self-tuning, lessons, scenarios,
> conviction mode, market-regime auto-adaptation, Telegram `/trade` +
> `/tradetask` mirrors, DEX quote/sign/reconcile (Jupiter/Uniswap).**
> Still planned: live DEX swap submission (needs wallet keys), full
> adversarial-debate tool loop. See `docs/trading/README.md` status table and
> `docs/trading/regime-adaptation.md` for the AI/algorithm division of labor.
> Date: 2026-09-05
> Author: developed with the Cryptograph AI copilot
> Scope: a fully independent, pluggable, dual-mode trading module ("Win-Win
> Accumulator") that buys dips and sells rips on multi-day/week/month horizons,
> is AI-driven (adversarial debate), algorithmically guarded against loss-making
> exits, and ships with a 1:1 CLI, Telegram control, backups, backtesting and a
> full fuzz/stress suite.
>
> **Execution venue: DEX-only (self-custody swaps).** Live orders are placed via
> decentralized swap aggregators (Jupiter on Solana, Uniswap/1inch on EVM) from
> self-custody wallets — never via KYC/CEX accounts. This keeps withdrawals to
> Belarusian P2P/private wallets clean and avoids centralized custody risk. See
> §2.1 and §8.7.
>
> **Autonomy & resilience (strict product requirements):** the trading worker
> must run autonomously with auto-restart, error resilience, hang protection,
> full fee accounting, and mathematical/logical verification of every AI
> decision. See §2.2.

---

## 1. Why this module exists (user requirements, verbatim intent)

- "бот накопления... торговать между просадками и пиками — типичная buy the dips".
- "он купил крипту — не может продавать этот объём по цене ниже... может только
  оставить нетронутой и ждать, а докупать просадку, потом при подъёме продавать".
- "речь не про часовые микропросадки, а про стратегию когда он может
  дни/недели/месяцы ждать, а потом продавать — автономный доход на основе
  колебаний рынка".
- "если неудачно — запоминает, обучается + монеты остаются на счету (DCA)".
- "я могу дать команду языковую ИИ — он должен понять и поправить параметры
  торговли" (NLP intent parsing).
- "должен помнить всё, логировать, решение должно быть решено экспертами на
  основе спора" (Bull vs Bear vs Arbiter).
- "задания: 'вот тебе 50$, задача — работаем по SOL'".
- "выгрузка в файл, бекапы в файл прямо в ТГ".
- "полностью развитая подключаемая 1:1 CLI система + автотесты".
- "полный набор фаззинга, граничных условий, нереалистичных и реалистичных
  ситуаций, стресс-тестирование на реальных ключах из .env".
- "лимиты: 'пока биток выше 65к не покупай' — чтобы он понимал ограничения".
- "автономное тестирование и AI-адаптация: дай временной отрезок истории,
  наложи реальный — выигрывал ли бот" (backtest).
- "не просто DCA, а часть средств бот крутит: повезёт — заработал, не повезёт —
  накопил" (Win-Win).
- "интеграция с текущими возможностями бота: новости, прогнозы, рынки, цифры".
- "отказоустойчивость и алгоритмические рамки: опасные операции (продажа в
  минус, сливы) математически и алгоритмически верифицированы и защищены".

---

## 2. Architecture & isolation topology

```
+-------------------------------------------------------------------+
|                      OPERATOR INTERFACES                          |
|   Telegram Bot (rich UX, backups)  |  Headless CLI & Fuzz Runner |
+------------------------------------+-----------------------------+
+------------------------------------v-----------------------------+
|              NLP INTENT & CONSTRAINT ENGINE                      |
|   - Semantic intent parser (LLM function-calling)                |
|   - Dynamic rule matrix ("BTC price ceiling <= 65k", "$50 SOL")  |
+------------------------------------+-----------------------------+
+------------------------------------v-----------------------------+
|              ADVERSARIAL MULTI-AGENT DEBATE ROOM                 |
|   +----------------+  +----------------+  +------------------+   |
|   | Bull Researcher|  | Bear Contrarian|  | Risk Chief/Arbiter|  |
|   | (dip depth,    |  | (news, macro,  |  | (hard invariant  |   |
|   |  volume, tech) |  |  exploit risk) |  |  gate + verdict) |   |
|   +----------------+  +----------------+  +------------------+   |
+------------------------------------+-----------------------------+
+------------------------------------v-----------------------------+
|              DETERMINISTIC LOT & INVARIANT ENGINE                |
|   - Specific-lot identification (immutable entry price + fees)   |
|   - No-loss enforcement guardrail (hard reject on negative PnL)  |
|   - Staggered tranche sizing (capital preservation governor)     |
+------------------------------------+-----------------------------+
|                +-------------------+-------------------+         |
+----------------v-------+        +--v--------------------+        |
|   SIMULATION ENGINE    |        |   LIVE ENGINE         |        |
|   - virtual ledger     |        |   - production store  |        |
|   - zero-capital learn |        |   - exchange router   |        |
|   - fuzz test bed      |        |   - atomic reconcile  |        |
+------------------------+        +-----------------------+        |
```

### Core principles

1. **Pluggable & decoupled.** Independent module + own worker loop. A crash or
   pause in the trading engine never disrupts the host bot (supervisor pattern
   already used by the bot: `supervisor::spawn_infallible`).
2. **Dual-database segregation.**
   - `trading_live.db` — production store: signed orders, wallet keys, active
     live lots, operator constraints.
   - `trading_sim.db` — virtual sandbox mirroring the live schema 1:1 for paper
     trading, backtesting and fuzzing. Same code path, different DB path.
3. **Specific-lot accounting** (not blended averages): each buy is an immutable
   lot with `entry_price`, `amount`, `fee_paid_usd`, `cost_basis_usd`. A lot can
   be `OPEN` (swing trade), `COLD_HOLD` (converted to long-term DCA stack when
   the market keeps falling) or `CLOSED` (sold at a profit).
4. **No-loss invariant is structural.** The execution layer refuses any exit
   whose net proceeds < cost basis. There is no "sell at a loss" path in code.
5. **AI proposes, engine disposes.** The debate produces a *verdict*; the
   deterministic engine validates it against invariants and operator
   constraints before any order is placed.

### 2.1 Execution venue: DEX-only (strict product requirement)

> "он должен использовать не какие-то там биржи, а выгодные децентрализованные
> или как их... просто свопы валют, но максимально легально, чтобы в Беларуси
> при выводе на P2P не возникло проблем"

The live engine **never** touches a centralized exchange (Binance/Bybit/OKX
have no trading API in this codebase and are read-only price providers).

- **Solana**: Jupiter Swap API (aggregator over all Solana DEXes) — buy/sell
  SOL, USDC, USDT, JitoSOL etc. from a self-custody wallet.
- **EVM**: Uniswap V2/V3 or 1inch aggregation for ETH/LINK/ERC-20 pairs.
- **BTC**: no native DEX for spot BTC; the module treats BTC as a hold-only
  asset (no live BTC swaps) unless the user later opts into a wrapped BTC
  (WBTC) route on an EVM chain.
- **Self-custody only**: keys live in `trading_live.db` (encrypted, same
  cipher as the bot's backups) or in a hardware wallet; the bot never holds
  user funds on a CEX.
- **Compliance**: no KYC, no fiat on-ramp inside the bot; the user moves funds
  to the wallet themselves. Withdrawals to Belarusian P2P stay private because
  the bot only ever produces on-chain swaps, not CEX withdrawals.
- **Slippage guard**: swap quotes are validated against the deterministic
  engine before signing (max slippage %, price-impact cap).

### 2.2 Autonomy & resilience (strict product requirements)

> "модули должны быть автономные с верификацией работы, с автоподнятием,
> с устойчивостью к ошибкам, с математической и логической верификацией ИИ,
> плюс учёт комиссий и прочих расходов, ну и защитой от зависаний"

1. **Autonomous worker with auto-restart.** The trading loop runs as its own
   task under the existing supervisor (`supervisor::spawn_infallible`): a panic
   or crash is caught, logged, and the worker is respawned — the host bot is
   never affected. A watchdog ping keeps the loop alive; a missed ping triggers
   restart.
2. **Hang protection.** Every external call (price fetch, DEX quote, model
   call) runs with a hard timeout. If a call hangs, the decision is aborted as
   `DEFER` (never executed on stale data), the error is logged, and the next
   cycle proceeds. No blocking I/O on the decision path without a deadline.
3. **Error resilience & graceful degradation.** Each tick is isolated: a bad
   tick (invalid price, locked DB, provider timeout) produces a logged skip,
   never a crash. The engine state (lots, allocations) is only mutated inside
   SQLite transactions; a failed tick leaves no partial writes.
4. **Mathematical & logical verification of AI decisions.** The debate room
   (Phase 4) may only *propose*; the deterministic engine (Phase 2, done) is
   the *disposer*: every proposal is re-checked against the no-loss invariant,
   operator constraints, circuit breakers and the capital boundary before any
   execution. A model can never bypass a guardrail — the engine rejects with a
   logged reason.
5. **Full fee accounting.** Every fill records `fee_usd` and `net_usd`;
   `cost_basis_usd = entry_price * amount + fee_paid_usd` is immutable per lot;
   the exit target is derived from the fee rate + profit margin so a "profit"
   is only ever *net* of fees. Realized profit is credited back to the
   allocation pool (compounding) via `credit_profit`.
6. **Idempotency & crash recovery.** Executions carry unique ids; SQLite WAL +
   `synchronous=NORMAL` survives SIGKILL mid-transaction (see §8.4). A restart
   re-reads the store and continues — no lost or double-executed orders.

---

## 3. Database schema (SQLite, one schema shared by live + sim)

```sql
-- Dynamic operator constraints & price ceilings (NLP-parsed)
CREATE TABLE IF NOT EXISTS operator_constraints (
    id TEXT PRIMARY KEY,
    mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
    asset TEXT NOT NULL,
    rule_type TEXT NOT NULL
        CHECK(rule_type IN ('MAX_PRICE_CEILING','MIN_PRICE_FLOOR',
                            'MAX_DRAWDOWN_STEP','PAUSE_BUY','PAUSE_SELL')),
    threshold_value REAL NOT NULL,
    raw_prompt TEXT NOT NULL,
    is_active INTEGER NOT NULL DEFAULT 1,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    expires_at TIMESTAMP
);

-- Strategy allocations ("$50 pool for SOL")
CREATE TABLE IF NOT EXISTS strategy_allocations (
    id TEXT PRIMARY KEY,
    mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
    asset TEXT NOT NULL,
    total_budget_usd REAL NOT NULL,
    deployed_usd REAL NOT NULL DEFAULT 0.0,
    available_usd REAL NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('ACTIVE','PAUSED','COMPLETED')),
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Individual trade lots (immutable entry, no-loss exit enforced)
CREATE TABLE IF NOT EXISTS trade_lots (
    id TEXT PRIMARY KEY,
    allocation_id TEXT REFERENCES strategy_allocations(id),
    mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
    pair TEXT NOT NULL,
    entry_price REAL NOT NULL,
    amount REAL NOT NULL,
    fee_paid_usd REAL NOT NULL,
    cost_basis_usd REAL NOT NULL,      -- (entry_price * amount) + fee_paid_usd
    target_exit_price REAL NOT NULL,   -- entry adjusted for profit margin + fees
    status TEXT NOT NULL CHECK(status IN ('OPEN','COLD_HOLD','CLOSED')),
    opened_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    closed_at TIMESTAMP
);

-- Multi-agent audit log (every decision, forever)
CREATE TABLE IF NOT EXISTS agent_debates (
    id TEXT PRIMARY KEY,
    lot_id TEXT REFERENCES trade_lots(id),
    mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
    market_context_json TEXT NOT NULL,
    bull_rationale TEXT NOT NULL,
    bear_rationale TEXT NOT NULL,
    arbiter_verdict TEXT NOT NULL,
    verdict_action TEXT NOT NULL CHECK(verdict_action IN ('BUY','PASS','DEFER')),
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Execution ledger (fills, fees, net proceeds)
CREATE TABLE IF NOT EXISTS trade_executions (
    id TEXT PRIMARY KEY,
    lot_id TEXT REFERENCES trade_lots(id),
    mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
    side TEXT NOT NULL CHECK(side IN ('BUY','SELL')),
    price REAL NOT NULL,
    amount REAL NOT NULL,
    fee_usd REAL NOT NULL,
    net_usd REAL NOT NULL,
    executed_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Backtest runs & per-run stats
CREATE TABLE IF NOT EXISTS backtest_runs (
    id TEXT PRIMARY KEY,
    mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
    symbol TEXT NOT NULL,
    start_date TEXT NOT NULL,
    end_date TEXT NOT NULL,
    initial_capital_usd REAL NOT NULL,
    final_capital_usd REAL NOT NULL,
    total_trades INTEGER NOT NULL,
    wins INTEGER NOT NULL,
    losses INTEGER NOT NULL,
    pnl_usd REAL NOT NULL,
    pnl_pct REAL NOT NULL,
    max_drawdown_pct REAL NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Strategy learning log (what the AI learned from a failed/winning trade)
CREATE TABLE IF NOT EXISTS strategy_lessons (
    id TEXT PRIMARY KEY,
    mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
    symbol TEXT NOT NULL,
    lesson TEXT NOT NULL,
    context_json TEXT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
```

### Notes

- `mode` column everywhere: the SAME code writes to either DB; the mode is
  stored per-row so a sim DB can never be confused with live.
- WAL pragma + `synchronous=NORMAL` (same as the host bot's `init_db`).
- All monetary values `REAL` (USD), amounts `REAL` (coin units). No integer
  rounding traps — but every comparison uses an epsilon (`1e-9`) and a
  `round_to_8` helper for display.

---

## 4. Adversarial deliberation & decision protocol

Every candidate action (buy / sell / hold) goes through the debate room:

1. **Bull Researcher** — argues FOR the action using: dip depth vs recent range,
   on-chain volume, funding rates, technical support levels, news catalysts.
2. **Bear Contrarian** — argues AGAINST using: macro risk, regulatory news,
   exploit/network risk, leverage/liquidation cascades, "this is a falling
   knife" reasoning. The Bear has veto power on fundamental risk.
3. **Risk Chief / Arbiter** — validates both arguments against:
   - the active `operator_constraints` (price ceilings, floors, pause flags);
   - the no-loss invariant (for exits);
   - the allocation budget (for buys);
   - volatility circuit breakers (spread, 15%/15min drop).
   Verdict: `BUY` | `PASS` | `DEFER` (DEFER = wait N hours, re-evaluate).

Implementation: reuse the existing MoE pipeline (`race_dynamic_generators` +
`run_dynamic_critic`) with role-specific prompts, OR a dedicated
`debate_room.rs` with 3 sequential `call_any_model` calls + a deterministic
arbiter gate. The deterministic gate ALWAYS has the final word (see §5).

### Tranche sizing (capital preservation governor)

- A `BUY` verdict is executed in fractional tranches (default 3 × 33%).
- Tranche 2/3 only deploy if the price is LOWER than the previous tranche
  (dip-buying) — never averaging UP.
- `strategy_allocations.available_usd` is the hard cap; the engine refuses to
  deploy more.

---

## 5. Algorithmic guardrails & mathematical invariants

### 5.1 No-loss invariant (hard, structural)

An exit order is blocked at the execution layer unless:

```
net_proceeds = amount * exit_price * (1 - taker_fee_rate) - fixed_fee
requires: net_proceeds >= cost_basis_usd * (1 + min_profit_margin)
```

If the asset keeps falling, the lot transitions to `COLD_HOLD` — it becomes a
long-term DCA position and is NEVER liquidated at a loss. The system simply
waits (days/weeks/months) and may buy more dips.

### 5.2 Volatility circuit breakers

- Buys halt if exchange bid-ask spread > 0.75%.
- Buys halt if price drops > 15% within 15 minutes (cascade protection).
- Sells halt if price is in free-fall (no liquidity to fill at fair price).

### 5.3 Capital boundary

- `deployed_usd + pending_buys <= total_budget_usd` — enforced in SQL and in
  code (belt and suspenders).
- No leverage, no shorts, no margin. Spot only.

### 5.4 Operator constraints (NLP-parsed, always checked)

- `MAX_PRICE_CEILING` (e.g. "don't buy BTC above 65k") → buy blocked when
  `price >= threshold`.
- `MIN_PRICE_FLOOR` → buy blocked when `price <= threshold` (falling-knife
  guard).
- `MAX_DRAWDOWN_STEP` → buy blocked when the price drawdown inside the
  observation window reaches `threshold` percent (cascade protection).
- `PAUSE_BUY` / `PAUSE_SELL` → hard pause flags.
- Constraints are stored with `raw_prompt` for auditability, carry an optional
  `expires_at`, and can be activated/deactivated via CLI/Telegram. `ALL` is a
  valid asset for global constraints. Expired constraints are ignored.

### 5.5 Profit routing & cumulative profit tracking

**Status: DONE.** `EngineConfig.profit_mode` decides what happens to realized
profit on every closed lot. The allocation tracks it cumulatively in
`total_profit_usd` (both paths), so the operator always knows the real
lifetime P&L of a pool regardless of where the money sits.

| Mode | Behavior | Allocation effect |
|------|----------|-------------------|
| `REINVEST` (default) | Every dollar of profit returns to the working pool | `credit_profit` → `total_budget_usd` grows, `available_usd` grows (compounding) |
| `SEPARATE` | Profit is parked in a dedicated reserve | `credit_profit_reserve` → `profit_reserve_usd` grows, working pool stays at its original budget |
| `PARTIAL:<f>` | `f` fraction reinvested, `1-f` parked | Split between `credit_profit` and `credit_profit_reserve` |

The reserve is **never deployable** — it is the operator's "take profit"
bucket. `available_usd` (what the engine may spend on new buys) only ever
grows from reinvested profit, never from the reserve.

---

## 6. Control interface & command matrix

### CLI (`cargo run -- trade ...`) — implemented, 1:1 with the Telegram commands

> The table below matches the **implemented** CLI (`src/trading/cli.rs`).
> `--mode live|sim` selects the engine; the default is `sim` (safe).

| Command | Arguments | Description |
|---------|-----------|-------------|
| `trade mode` | `[--live\|--sim]` | Show / switch the active execution engine + its DB file |
| `trade allocate` | `<SYMBOL> <USD> [--mode sim\|live]` | Spawn an isolated capital pool (e.g. `trade allocate SOL 50`) |
| `trade rule` | `<SYMBOL> <TYPE> <VALUE> [--mode sim\|live]` | Attach a constraint. `TYPE` ∈ `ceiling\|floor\|drawdown\|pause-buy\|pause-sell` (aliases: `max_price_ceiling`, `min_price_floor`, `max_drawdown_step`, `pause_buy`, `pause_sell`) |
| `trade tick` | `<SYMBOL> <PRICE> [--bid B] [--ask A] [--execute]` | Run one deterministic engine cycle. Without `--execute`: plans only, no state change |
| `trade buy` | `<SYMBOL> <PRICE> [--mode sim\|live]` | Force plan + execute a buy (prints skip reason if blocked) |
| `trade sell` | `<LOT_ID> <PRICE> [--mode sim\|live]` | Force plan + execute a sell (no-loss invariant gated) |
| `trade status` | `[--asset <SYM>]` | Open swing lots, cold stack, free cash per allocation |
| `trade export` | `--format <csv\|json>` | Full trade ledger + decision audit trail |
| `trade backup` | `<dest.db>` | Hot SQLite snapshot to a file (Telegram upload is a planned `/trade backup`) |
| `trade backtest` | `--symbol <SYM> --from <date> --to <date> [--capital <USD>] [--ceiling P] [--floor P] [--margin M] [--fee F] [--tranches N] [--crash-window S] [--walk-forward]` | Replay real history through the engine (plan §7) |
| `trade backtest --batch` | `--symbols A,B,C --from <date> --to <date> [--window-days N] [--step-days N]` | Rolling-window sweep, hundreds of runs, aggregate summary |
| `trade lessons` | `[--limit N]` | Show strategy lessons the AI learned |
| `trade backtests` | `[--limit N]` | Show recorded backtest runs |
| `trade tune` | `[--dry-run] [--model ALIAS] [--limit N]` | AI self-tuning: model proposes `EngineConfig` changes from backtests + lessons; deterministic gate rejects garbage |
| `trade config` | `[--set FIELD VALUE]` | Show current config + tuning history, or apply a validated manual override |
| `trade strategies` | | List the strategy catalog (builtin + trained) |
| `trade strategy apply` | `<NAME> [--mode sim\|live]` | Apply a strategy profile: sets the active engine config + default constraints |
| `trade strategy train` | `[--symbols A,B,C] [--from DATE] [--to DATE] [--window-days N] [--step-days N] [--capital USD] [--top N] [--grid-full] [--force]` | Sweep candidate configs over historical windows and persist the best as trained profiles (plan §7.5) |
| `trade strategy delete` | `<NAME>` | Delete a stored (trained/manual) profile |
| `trade pause` / `trade resume` | `<alloc-id>` | Pause / resume a specific allocation |

### Telegram commands (mirror the CLI)

- `/trade` — status dashboard (inline buttons).
- `/trade allocate 50 SOL` — NLP: creates a $50 SOL pool.
- `/trade rule пока биток выше 65к не покупай` — NLP constraint.
- `/trade backup` — sends the DB snapshot file to chat.
- `/trade export csv` — sends the ledger file.
- `/trade pause` / `/trade resume`.
- `/trade log` — recent decisions + debates (audit trail in chat).

### NLP intent parsing targets

- "Don't buy BTC above $65k" → `MAX_PRICE_CEILING = 65000` for BTC.
- "Allocate $50 to SOL, buy on dips" → $50 pool for SOL + 3-stage tranche plan.
- "Send database backup to chat" → `sqlite3_backup()` + upload `.db` to Telegram.
- "Pause trading" → global `PAUSE_BUY` + `PAUSE_SELL`.
- "Work only on ETH for a month" → per-asset allocation + expiry.

Parser: reuse the existing watcher-tier planner pattern (`plan_web_search`):
a cheap model emits structured JSON (`{intent, asset, amount, threshold,
rule_type}`), validated by a deterministic schema check. Invalid/unparseable
input → explicit "I didn't understand, try: ..." reply. NEVER executes on
unparsed text.

---

## 7. Backtesting & AI adaptation loop

> **Status: DONE (Phase 6).** `src/trading/backtest.rs` — replay engine,
> batch sweeps, walk-forward, lessons. Validated on real Binance daily klines
> 2020-01-01 → 2026-08-01 (BTC/ETH/SOL/LINK).

### 7.1 Backtest engine

- Input: symbol, date range, initial capital, optional constraints.
- Data source: **Binance daily klines, paginated** (1000 candles/request via
  `startTime`/`endTime` loop) — the local `price_history` table only keeps
  ~30 days and is NOT used. `fetch_history_blocking` wraps the async fetch
  with `futures::executor::block_on` because the CLI runs inside the app's
  tokio runtime (a nested `Runtime::new()` panics).
- The engine replays prices day-by-day through the REAL decision pipeline:
  - deterministic rules first (tranche levels, no-loss invariant, constraints);
  - optional AI debate (configurable: `--ai-debate on|off` — off for fast
    deterministic runs, on for realistic model-in-the-loop runs).
- Output: `backtest_runs` row + a printed report (trades, wins/losses, PnL,
  max drawdown) + optional CSV export.
- CLI: `trade backtest --symbol BTC --from 2020-01-01 --to 2026-08-01
  --capital 1000 [--ceiling P] [--floor P] [--margin M] [--fee F]
  [--tranches N] [--crash-window S] [--walk-forward]`.
- Batch: `trade backtest --batch --symbols BTC,ETH,SOL,LINK --from ... --to
  ... --window-days 90 --step-days 30` → rolling windows, one fresh sim store
  per run, all runs + lessons persisted to `trading_sim.db`.

**Findings (default config: margin 1%, fee 0.3%, 3 tranches, crash window 7d):**

| Symbol | Windows | Profitable | Avg PnL/window | Closed trades | Win rate |
|--------|---------|-----------|----------------|---------------|----------|
| BTC    | 78      | 45 (58%)  | +3.28%         | 1364          | 100%     |
| ETH    | 78      | 43 (55%)  | +5.42%         | 1475          | 100%     |
| LINK   | 78      | 42 (54%)  | +4.39%         | 1485          | 100%     |
| SOL    | 73      | 35 (48%)  | +8.59%         | 1273          | 100%     |

- Full-horizon BTC 2020→2026: $1000 → $4294 (+329%), 220 trades, 0 realized
  losses, max DD 75.99% (2022 bear).
- The no-loss invariant holds: **0 realized losses across ~5600 closed trades**.
  Unprofitable windows are bear-market stretches where lots sit in COLD_HOLD
  and the window ends with unrealized drawdown — exactly the "не повезло —
  накопил" behavior.
- Two engine bugs found & fixed while validating:
  1. **Dust-lot churn**: rounding remainders (~1e-8 USD) after `deploy_funds`
     produced 1e-13-coin lots and eventually a false "insufficient funds"
     error. Fix: `EngineConfig.min_tranche_fraction` (default 0.5% of pool) —
     buys below that are skipped.
  2. **Float rounding overdraw**: `open_lot` recomputes cost basis with
     `round_to_8`, which can exceed `available` by ~1e-8. Fix: clamp the
     stored cost basis to the pool when the diff ≤ 1e-6 USD (real overdraws
     still rejected).

### 7.2 Walk-forward validation

- Split history: train on `[start, mid]`, validate on `[mid, end]` — CLI flag
  `--walk-forward` runs both and prints them.
- "Наложи реальный": after a backtest on a historical window, continue the
  same engine state into the live/sim present and compare the model's
  decisions against what actually happened.

### 7.3 Strategy lessons

- Every closed lot (win or loss) produces a `strategy_lessons` row: what the
  AI expected vs what happened, what to repeat/avoid (capped at 10 lots/run +
  one run-summary lesson).
- Lessons are injected into future debate prompts (like memos for the trading
  brain) so the bot "обучается" over time. CLI: `trade lessons [--limit N]`.

### 7.4 AI self-tuning (tuner)

**Status: DONE.** `src/trading/tuner.rs` — "the model learns, the engine
executes". The deterministic engine decides *whether* to trade; the tuner
decides *the settings*.

- Input: recent `backtest_runs` + `strategy_lessons` from the sim DB,
  summarized into a prompt with the current `EngineConfig` and the sane bounds.
- The model returns a partial JSON proposal (`TunerProposal`); `None` fields
  mean "keep current".
- **Deterministic gate is the last word**: every field is validated against
  tuner bounds (tighter than the engine's hard validation) — margin
  `[0, 0.25]`, fee `[0, 0.05]`, tranches `[1, 12]`, min-tranche-fraction
  `[0, 0.5]`, spread `[0, 5]%`, crash-drop `[1, 60]%`, crash-window
  `[60, 30d]`. NaN, garbage, out-of-bounds → proposal rejected, current
  config kept. The tuner never panics and never applies garbage.
- `apply_proposal` re-validates the fully merged config (`validate()` +
  `check_tuner_bounds()`) so proposals built outside the parse gate can't set
  absurd values; the CLI `config --set` path uses the same gate.
- Accepted configs are persisted to `engine_configs` (history kept for audit).
  CLI: `trade tune [--dry-run]`, `trade config [--set FIELD VALUE]`.
- Rate-limit resilience: `MAX_ATTEMPTS = 2` model calls before giving up;
  any API error falls back to the current config.

### 7.5 Pre-trained strategy catalog & training pipeline

**Status: DONE.** `src/trading/strategies.rs` — the engine never starts from a
blank slate; it ships with learned defaults and can retrain on demand.

**Strategy profiles.** A profile is a named, validated `EngineConfig` plus
optional default operator constraints, stored in the `strategy_profiles` table
(engine config + constraints as JSON snapshots; `name` is the primary key).
Sources: `builtin` (ships with the binary), `trained` (from the pipeline),
`manual`.

Built-in catalog:

- `Win-Win-Accumulator` — flagship: margin 1%, 3 tranches, `PARTIAL:0.5`
  profit routing (half compounds, half parks in reserve).
- `Conservative-DCA` — margin 0.5%, 4 tranches, `SEPARATE` profit routing;
  for flat-to-bearish regimes.
- `Aggressive-Dip-Buyer` — margin 2%, 6 tranches, `REINVEST`; more frequent
  dip buys with compounding.
- `Euphoria-Protector` — margin 3%, 2 tranches, tight spread + aggressive
  crash breaker, `SEPARATE`; prevents buying tops.

**Applying a profile** (`trade strategy apply <NAME> [--mode live|sim]`)
persists its engine config as the active one (`source = "strategy:<name>"`)
and adds its default constraints. `trade tick/buy/sell` load the active config
from `engine_configs` instead of hardcoded defaults, so an applied strategy
actually takes effect.

**Training pipeline** (`trade strategy train`): sweeps a candidate grid
(`--grid-full` = 360 configs, default small = 54) over rolling windows per
symbol. History is fetched once per symbol and sliced per window — no per-run
API calls. Each candidate is scored:

```
score = avg_pnl_pct − 0.5·avg_max_dd_pct − 20·cold_ratio + 8·profitable_ratio + 5·win_rate
```

- `avg_pnl_pct` — mean window PnL (reward).
- `avg_max_dd_pct` — mean window drawdown (penalize).
- `cold_ratio` — fraction of lots stuck in COLD_HOLD, i.e. frozen capital
  (penalize).
- `profitable_ratio` — fraction of windows ending positive (reward
  consistency).
- `win_rate` — realized-trade win rate (reward).

The top `--top N` configs are persisted as `source="trained"` profiles
(`Trained-<rank>-m<margin>-t<trans>-<mode>-c<crash>`; crash params are part of
 the name so configs that differ only in the circuit breaker stay distinct).
Idempotent: skips when trained profiles already exist unless `--force`.
Intermediate runs are NOT persisted — only the winners land in the store.

**Walk-forward discipline.** The enterprise test suite trains on a bull window
and validates out-of-sample on a bear window, asserting the trained config
never realizes a loss (the no-loss invariant holds out-of-sample).

---

## 8. Testing, fuzzing & verification suite

> Live-model smoke tests use the real keys already present in `.env` (GEMINI,
> CMC, TAVILY, EXA, GROQ, CEREBRAS, NVIDIA, MISTRAL, COHERE, CLOUDFLARE,
> DEEPSEEK). No exchange keys are required for phases 1–7.

### 8.1 Unit tests (deterministic, no network)

- No-loss invariant: exit at loss → rejected; exit at profit ≥ margin → allowed.
- Tranche sizing: never average up; budget cap enforced.
- Constraint engine: ceiling/floor/pause parsing + enforcement.
- Lot lifecycle: OPEN → COLD_HOLD → CLOSED transitions.
- Ledger math: fees, net proceeds, cost basis (epsilon comparisons).
- NLP parser: valid intents → structured JSON; garbage → explicit rejection.
- Strategy catalog: builtin profiles validate + JSON round-trip; store CRUD;
  `apply_profile` sets the active config + constraints.
- Training pipeline: synthetic sweeps produce `trained` profiles, rank the
  profitable config first, and are idempotent (`--force` retrains).
- Stress: flash-crash (≈90% drop) → 0 realized losses, lots → COLD_HOLD;
  whipsaw → 0 realized losses; wide spread blocks buys.
- Fuzz: 50 random configs within tuner bounds never panic or realize losses;
  garbage configs rejected by validation.
- Walk-forward: train on a bull window, validate on a bear window — no
  out-of-sample realized losses.

### 8.2 Deterministic exchange mocking

- Mock fills: partial fills, stale order-book quotes, network timeouts.
- The engine must handle partial fills without double-spending the budget.

### 8.3 Extreme downside stress tests

- Instant 50–90% drops: engine halts sells, protects inventory, prevents
  balance overdrafts, converts lots to COLD_HOLD.
- Flash-crash + recovery: lots bought on the dip, sold on the rip.

### 8.4 Process interruption fuzzing

- SIGKILL mid-transaction → SQLite WAL recovery on restart; no lost or
  double-executed orders (idempotency keys on executions).

### 8.5 Realistic scenario runs (existing `scenarios/` infra)

- `crash.json`, `pump.json`, `euphoria.json`, `accumulation.json` — reuse for
  the debate room: verify Bull/Bear/Arbiter produce sane verdicts under each
  regime and that the deterministic gate overrides bad AI advice.

### 8.6 Live-model smoke tests (keys in `.env`)

- `cargo run -- trade test --fuzz --scenarios 5` with real model calls to
  observe what the AI actually decides (the user explicitly asked: "потестируй,
  посмотри как модели работают, что он будет делать").

---

## 9. Integration with existing bot capabilities

- **Prices/quotes**: `CryptoClient::get_quotes` (CMC/CoinGecko/CoinCap/CC +
  exchange fallbacks) — the same data the /ask pipeline uses.
- **News/macro intel**: `sqlite.get_news_summary(sym)`,
  `get_macro_summary("GLOBAL"|"DERIVATIVES"|"BTC_NETWORK")` — injected into the
  Bull/Bear prompts as ground truth.
- **Fear & Greed**: `fetch_fear_and_greed_index()` — regime context for the
  arbiter.
- **On-chain**: `get_onchain_metrics` (TVL, funding, OI) — Bull/Bear evidence.
- **Strategy layer**: reuse `strategy.rs` `Regime` classification to seed the
  debate (Accumulate → bias toward BUY on dips; Euphoria → bias toward SELL
  proposals / no new buys).
- **Supervisor**: run the trading worker via `supervisor::spawn_infallible`
  with its own tick interval (e.g. every 30 min scan + event-driven on alerts).
- **Backups**: reuse `db::migration::encode/decode` envelope pattern for
  trading DB export/import.

---

## 10. Implementation order (phases, each independently shippable)

| Phase | Scope | Gate |
|-------|-------|------|
| 0 | `docs/trading/` scaffolding + this plan | review |
| 1 | **Schema + store**: `src/trading/store.rs` (both DBs), CRUD for constraints/allocations/lots/debates/executions/lessons | `cargo test` (done) |
| 2 | **Deterministic engine**: `src/trading/engine.rs` — lot lifecycle, no-loss invariant, tranche sizing, circuit breakers, constraint enforcement, `credit_profit` compounding | `cargo test` (done, 45 trading tests) |
| 3 | **Sim engine + CLI**: `trade` command tree (mode/allocate/rule/tick/buy/sell/status/export/backup/pause), sim ledger | CLI smoke test (tick/buy/sell done) |
| 4 | **Debate room**: Bull/Bear/Arbiter prompts + deterministic arbiter gate, `agent_debates` logging | scenario runs |
| 5 | **NLP intent parser**: constraint + allocation parsing, validation, rejection UX | unit tests + live-model smoke |
| 6 | **Backtest + walk-forward**: replay engine, `backtest_runs`, lessons | backtest on real history (done — 307-window sweep, 0 realized losses) |
| 7 | **Telegram integration**: `/trade` commands, inline keyboards, backup-to-chat, alert delivery | manual TG test |
| 8 | **Fuzz/stress suite**: mock exchange, crash scenarios, SIGKILL recovery, live-model smoke | full suite green |
| 8.7 | **DEX router (live)**: Jupiter/Uniswap/1inch swap execution, slippage guard, self-custody keys | gated on user approval + wallet keys |

### Anti-scope-creep guard

- LIVE exchange routing (real order placement) is the LAST step and is gated on
  user approval + exchange API keys. Everything before it runs in SIM mode.
- The paper-trading sandbox (`todo.md`, `docs/trading/`) remains gated; this
  module is the concrete implementation of that concept.

---

## 11. Risks & mitigations

| Risk | Mitigation |
|------|------------|
| AI debate is slow/expensive | Deterministic rules run first; AI debate is optional per action and bounded (1 round, cheap models for Bull/Bear, watcher-tier arbiter). |
| Model hallucinates a "great" buy at a peak | Deterministic gate: regime + constraints + tranche rules override the verdict. |
| Partial fills / exchange glitches | Mock-exchange tests; idempotency keys; budget reconciliation after every fill. |
| DB corruption mid-trade | WAL + synchronous=NORMAL; idempotent re-apply; backup command. |
| User changes constraints mid-trade | Constraints are re-checked at every decision, not cached. |
| Scope creep into live trading | Live router is phase 8 and gated on explicit approval + keys. |

---

## 12. Open questions for future releases

1. Default profit margin per lot (currently 1% net after fees) and default
   tranche count (3 × 33%) — tune via `EngineConfig`.
2. Should the trading worker run continuously in the bot process (supervisor
   loop, e.g. every 30 min), or only via CLI/Telegram commands for now?
3. Backtest data depth: local `price_history` only, or pull Binance klines
   (needs network + possibly API key)?
4. Live DEX router (Phase 8.7): which chains first — Solana (Jupiter) or EVM
   (Uniswap/1inch)? Wallet keys are required and will be stored encrypted.