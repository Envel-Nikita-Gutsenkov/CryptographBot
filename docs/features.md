# Cryptograph Features

Cryptograph is packed with professional financial tracking features designed for absolute privacy and maximum usability.

## 1. Conversational FSM Wizards

To ensure an exceptionally smooth user experience, Cryptograph uses stateful step-by-step dialogue wizards instead of complex command syntaxes.

### 🔔 Price Alert Wizard
Accessed via the **Add Alert** button inside the Alerts menu:
1. **Step 1**: The user is prompted to type a cryptocurrency symbol (e.g. `BTC`).
2. **Step 2**: An inline keyboard is rendered, offering conditional options:
   - `📉 Price Below`
   - `📈 Price Above`
   - `⬇️ Drop % over period`
   - `⬆️ Rise % over period`
3. **Step 3**:
   - If a percentage condition is picked, the user selects a timeframe (`1h`, `4h`, `24h`, `7d`, `30d`, `90d`) and then enters the change percentage threshold (e.g. `5` for 5%).
   - If a price condition is picked, the user enters the absolute USD price threshold (e.g. `150000`).

### 👛 Wallet Registration Wizard
Accessed via the **Add Wallet** button inside the Wallets menu:
1. **Step 1**: An inline keyboard is displayed to pick the wallet's category:
   - `🧊 Cold` (Hardware or paper wallet)
   - `🔥 Hot` (Software or mobile app)
   - `🏦 Exchange` (Centralized exchange)
2. **Step 2**: The user enters the wallet specification.
   - For simple non-monitored tracking: `[Wallet Name]` (e.g. `MyBinance`).
   - For automatic blockchain tracking: `[Wallet Name] [SYM] [ADDRESS]` (e.g. `Trezor BTC bc1q...`).

---

## 2. Market Intelligence & Advice

The `/advice` command generates sophisticated market intelligence reports using live metrics:
- **Market Sentiment Index**: Evaluates aggregate market trends and calculates the **Fear & Greed Index** based on trading volumes, BTC dominance, volatility, and trend directions.
- **DCA/Buy Recommendations**: Analyzes your average purchase price relative to current asset valuations and provides suggestions on whether to hold, realize profits, or accumulate (DCA) during corrections.
- **Halving Timelines**: Displays estimates for the next Bitcoin halving event (target: 2028-2029) to assist in planning multi-year exits.

---

## 3. High-Fidelity Infographic Charts

Cryptograph generates premium charts inside the chat interface using the native `Plotters` engine without using external charting servers:
- **`/pnl` (Profit & Loss Timeline)**: A time-series chart mapping investment growth, current valuation, and cumulative profits/losses.
- **`/distribution` (Asset Allocation)**: A pie chart mapping portfolio weight distribution by USD value.
- **Top Movers**: Horizontal bar charts comparing 24-hour performance dynamics across the top market cap coins.

---

## 4. Anti-Spam Alerts & Quiet Hours

- **Anti-Spam Engine**: Alerts require the threshold condition to be met for at least **two consecutive checks** (5 minutes apart) to prevent false alerts caused by quick price wicks.
- **Quiet Hours**: Users can set time periods during which low-severity alerts are queued and silenced (e.g., `23:00 - 07:00 UTC`), protecting sleep. High-priority alerts can bypass this filter.

---

## 5. Trading Module — Win-Win Accumulator (`trade` CLI)

A pluggable, dual-mode trading subsystem (simulation + live) that buys dips in
tranches and never realizes a loss:

- **`trade mode [--live|--sim]`** — switch between the paper engine
  (`trading_sim.db`) and the live engine (`trading_live.db`).
- **`trade allocate <SYMBOL> <USD>`** — create an isolated capital pool
  (e.g. `trade allocate SOL 50`).
- **`trade rule <SYMBOL> <TYPE> <VALUE>`** — operator limits: `ceiling`,
  `floor`, `drawdown`, `pause-buy`, `pause-sell` (e.g. "don't buy BTC above
  $65k" → `trade rule BTC ceiling 65000`).
- **`trade tick / buy / sell`** — deterministic engine cycles with the
  **no-loss invariant**: a sell below cost basis + margin is hard-rejected and
  the lot becomes `COLD_HOLD` (long-term DCA) until price recovers.
- **ATH guard** — the engine refuses to buy near the recent all-time high
  (`ath_lookback_days`, `max_buy_from_high_pct`), parking cash instead of
  chasing tops (verified: no buy at the Nov-2021 peak).
- **`trade backtest`** — replay real history through the engine (multi-source
  failover: Binance → CryptoCompare → CoinGecko); `--batch` sweeps rolling
  windows over 2020–2026 for BTC/ETH/SOL/LINK (win-rate 100%, zero realized
  losses by design). Windows with too few candles (pre-listing noise) are
  skipped.
- **`trade strategies` / `strategy apply|train|delete`** — builtin and trained
  strategy profiles; `strategy train --grid-full --threads N` parallelizes the
  sweep; **`trade seed`** bundles the best profiles + lessons + stats into
  `data/trading_sim_seed.db`, auto-copied into place on first run (never start
  from zero); **`trade tune`** lets a model propose config changes gated by a
  deterministic validator.
- **`trade recommend ...`** — commander mode: the bot plans a trade with full
  guardrail checks and records it as PENDING advice; the operator executes on
  their own exchange and reports back (`recommend execute` or free text via
  `recommend parse` — "купил ETH за 1800"); the bot re-checks every guardrail
  at the actual fill price and records the outcome.
- **`trade task ...`** — standing trade tasks with a monthly budget, alerts/day
  cap, and tranches (`task add|list|pause|resume|advise`); `task sim`/`task
  grid` replay the config over real history.
- **`trade fee --buy P --sell P`** — per-mode exchange fees, accounted for in
  the no-loss invariant and advisor sizing.
- **`trade export --format csv|json`**, **`trade backup <dest.db>`**,
  **`trade lessons`**, **`trade backtests`**, **`trade status`** — full ledger,
  audit trail, and learning memory.

> Telegram `/trade` and `/tradetask` mirrors are implemented; DEX execution
> (Jupiter/Uniswap) is implemented up to quote/sign/reconcile — live swap
> submission needs wallet keys. See `docs/trading/`.

---

## 5b. News Layer — Pre-Digested Summaries + On-Demand Details

- Raw articles are stored per symbol (`news_articles`, dedup by URL, 7-day
  retention) and AI-verified summaries are appended to `news_summary_history`
  (30-day retention) — the AI prompt only ever sees compact fresh summaries
  (24h staleness guard), never raw feeds.
- `news summary|history|articles|cleanup` CLI exposes the full store for
  on-demand "extended results" without burning tokens on re-processing raw
  articles.

---

## 6. Lazy AI Model Ranking (`models` CLI)

Cryptograph continuously discovers and ranks free models across all providers
without burning rate limits:

- **`models refresh`** — upsert `models.json` + OpenRouter free models into the
  registry (fingerprint-aware).
- **`models rank`** — benchmark **at most 3 models per run** (30-day cooldown,
  fingerprint-change detection, underperformer re-tests) with a non-OpenRouter
  judge; assigns roles (`main`/`critic`/`news`/`watcher`) and writes
  `models.openrouter.json`.
- **`models stats [days]`** — per-provider calls, error %, latency, quality
  (quality averaged over benchmark calls only).
- **`models calibrate`** — measure the judge's baseline (must stay ≥ 4.0).
- Hard budgets: DeepSeek monthly cap (3M tokens / 150 req) and OpenRouter daily
  cap (300 req/day), enforced atomically and mirrored in SQLite.

---

## 7. CLI Diagnostics & Scenarios

- **`ask <user_id|demo> "question"`** — replay the full `/ask` pipeline with
  prompts, web searches, drafts, and critic.
- **`goal <demo|user_id> [-v]`** — replay a proactive goal scan, stage by stage.
- **`scenario <file.json> [--fast]`** — deterministic what-if: a scenario file
  replaces market/news/macro data and the **real** `/ask` + goal-scan pipeline
  runs against it (anti-FOMO verified: no buy at peak in `euphoria.json`).
- **`analyze <SYM>`**, **`search "query"`**, **`wipe-knowledge`** — deep
  analysis, search-flow inspection, and knowledge-base reset (keeps users,
  goals, memos, budgets).
