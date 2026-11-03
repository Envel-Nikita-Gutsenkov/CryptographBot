# Trading Module — Documentation Index

> **Branch:** `main`
> **Status:** CLI engine **implemented** (sim + live modes, backtesting, scenarios,
> strategy catalog, self-tuning, ATH guard, commander/advice mode, trade-task
> advisor, conviction mode, market-regime auto-adaptation, seed-DB bundling,
> DEX quote/sign/reconcile). Telegram `/trade` mirror **implemented**; live DEX
> swap submission still needs wallet keys (see "Implementation status" below).

This directory contains the architecture specification and implementation notes
for Cryptograph's trading subsystem — the **Win-Win Accumulator**: buy dips in
tranches, never realize a loss (underwater lots become long-term DCA holdings),
and take profit only when the no-loss invariant is satisfied.

---

## Implementation status

| Area | Status |
|---|---|
| `trade` CLI (mode, allocate, rule, tick, buy, sell, status, export, backup) | ✅ implemented |
| Operator constraints (price ceilings/floors, pause-buy, drawdown step) | ✅ implemented |
| No-loss invariant + COLD_HOLD transition | ✅ implemented |
| ATH guard (no buying near recent all-time high) | ✅ implemented |
| Fee accounting + profit routing (REINVEST / SEPARATE / PARTIAL) | ✅ implemented |
| Backtesting (`backtest`, `backtest --batch`, walk-forward, multi-source history failover) | ✅ implemented |
| Strategy catalog (`strategies`, `strategy apply/train/delete`) | ✅ implemented |
| Parallel training (`strategy train --grid-full --threads N`) + seed-DB bundling (`seed`) | ✅ implemented |
| Self-tuning (`tune`) + lessons + recorded runs | ✅ implemented |
| Commander / advice mode (`recommend buy|sell|list|execute|reject|expire|parse`) | ✅ implemented |
| Trade-task advisor (`task add|list|pause|resume|advise|sim|grid`) | ✅ implemented |
| Deterministic scenarios (`scenarios/*.json` + `--fast`) | ✅ implemented |
| Telegram `/trade` mirror (list/pnl/fee/allocate/constraints/rule) | ✅ implemented |
| Telegram `/tradetask` mirror (list/add/pause/resume/advise) | ✅ implemented |
| Conviction mode (`--grid-conviction`, `--min-dip`, `Conviction-Dip-Buyer` profile) | ✅ implemented |
| Market-regime auto-adaptation (`regime.rs`, `regime_profiles`, `trade regime`, `strategy regimes`) | ✅ implemented |
| Market-state analysis (`trade analyze`, locality block for `/ask`) | ✅ implemented |
| DEX quote + manual sign + fill reconciliation (`trade dex quote|sign|reconcile`) | ✅ implemented (needs wallet keys for live swap submission) |

The canonical engineering plan is
[`TRADING_MODULE_PLAN.md`](TRADING_MODULE_PLAN.md) (kept as a historical design
document). The CLI is the 1:1 testing surface for every trading command; the
repository test suite (`cargo test`) is the source of truth for verified
behaviour.

---

## Document Map

| Document | Description | Status |
|---|---|---|
| [`TRADING_MODULE_PLAN.md`](TRADING_MODULE_PLAN.md) | Historical engineering plan (schema, invariants, command matrix, backtesting, testing) | 📜 historical |
| [`architecture.md`](architecture.md) | Strategist–executor separation, component diagram, data flow | 🧩 partial — engine and risk layers shipped; the continuous sandbox loop is design |
| [`data-models.md`](data-models.md) | Older `VirtualPortfolio` / `ActiveTrigger` / `TradingJournal` shapes | ⏳ design — the shipped schema is in [database.md](../database.md) §4 |
| [`execution-engine.md`](execution-engine.md) | Tick evaluation, trade execution, consensus pricing | 🧩 shipped equivalent — event-driven ticks, no autonomous loop |
| [`risk-management.md`](risk-management.md) | Veto rule engine, safe ranges, hazard detection | 🧩 partial — operator constraints and the no-loss invariant are shipped; the veto engine is design |
| [`ai-tools.md`](ai-tools.md) | JSON function-calling toolset for the strategist | ⏳ design |
| [`moe-pipeline.md`](moe-pipeline.md) | Double-checking, adversarial debate, decision proofs | 🧩 partial — sell-debate and critic exist; the full debate loop is design |
| [`fee-accounting.md`](fee-accounting.md) | Fee/slippage simulation and real P&L | 🧩 partial — `exchange_fees` drives the invariant and advisor sizing; full simulation is design |
| [`user-commands.md`](user-commands.md) | Telegram `/trade` + `/tradetask` mirrors, CLI 1:1 surface | ✅ implemented |
| [`dex-execution.md`](dex-execution.md) | Jupiter/0x quotes, manual sign, fill reconciliation, Belarus P2P on-ramp | ✅ implemented (live swaps need wallet keys) |
| [`regime-adaptation.md`](regime-adaptation.md) | Market-regime classifier, auto config selection, AI vs algorithm roles | ✅ implemented |
| [`vulnerability-response.md`](vulnerability-response.md) | 7 critical vulnerabilities + mitigations | ✅ current |
| [`logging-analytics.md`](logging-analytics.md) | Log export, performance analytics, attribution | 🧩 partial — `trade export`, backtest runs and lessons are shipped; the journal is design |
| [`model-training.md`](model-training.md) | Performance review, strategy evolution, A/B testing | 🧩 partial — `strategy train`, `tune`, `lessons`, `training_stats` are shipped |
| [`strategies.md`](strategies.md) | Strategy playbook: entry/exit rules, expected metrics, anti-patterns | 🧩 partial — catalog and profiles are shipped; the playbook is advisory |
| [`agent-architecture.md`](agent-architecture.md) | Multi-step agent thinking (≤20 steps), sub-agent delegation | ⏳ design |
| [`introspection.md`](introspection.md) | Live brain view, decision proofs, stats dashboard | 🧩 partial — `trade analyze`, regime and recorded runs are shipped |
| [`stability.md`](stability.md) | Fault tolerance, crash recovery, graceful degradation | 🧩 partial — supervision, checkpointing and schema migration are shipped |
| [`testing.md`](testing.md) | Unit tests, CLI scenarios, historical backtest, shadow, fuzzing | 🧩 partial — unit tests, scenarios and backtests are shipped; shadow/fuzzing are design |
| [`security.md`](security.md) | Dead man's switch, audit trail, emergency stop | 🧩 partial — ACL, audit tables and pause controls are shipped |

> **Legend.** ✅ implemented = the command or subsystem exists and is covered by the
> test suite. 🧩 partial = some of the described behaviour ships under a different
> design (the shipped schema lives in [database.md](../database.md)). ⏳ design =
> target behaviour that is not in the code yet. 📜 historical = kept for context,
> not a description of the current system.
>
> The shipped behaviour is documented in
> [`TRADING_MODULE_PLAN.md`](TRADING_MODULE_PLAN.md) and the
> [command reference](../commands.md).

---

## Quick Reference (implemented CLI)

```
cargo run -- trade mode [--live|--sim]          # active engine + DB file
cargo run -- trade allocate <SYMBOL> <USD>      # isolated capital pool
cargo run -- trade rule <SYMBOL> <TYPE> <VALUE> # ceiling/floor/pause-buy/drawdown
cargo run -- trade tick <SYM> <PRICE> [--execute]
cargo run -- trade buy <SYM> <PRICE>            # forced buy (invariant-gated)
cargo run -- trade sell <LOT_ID> <PRICE>        # profit-taking (no-loss gated)
cargo run -- trade status [--asset SYM]
cargo run -- trade export --format csv|json     # full ledger + audit trail
cargo run -- trade backup <dest.db>             # hot SQLite snapshot
cargo run -- trade seed                         # bundle profiles+lessons+stats into
                                                # data/trading_sim_seed.db (first-run preload)
cargo run -- trade backtest --symbol BTC --from 2025-09-01 --to 2026-09-01
cargo run -- trade backtest --batch --symbols BTC,ETH,SOL,LINK --from 2020-01-01 --to 2026-09-01
cargo run -- trade strategies / strategy apply|train|delete
cargo run -- trade strategy train --grid-full --threads 16   # parallel sweep
cargo run -- trade tune [--dry-run]             # AI self-tuning
cargo run -- trade config [--set FIELD VALUE]   # engine config + history
cargo run -- trade fee [--buy P] [--sell P]     # per-mode exchange fees
cargo run -- trade recommend buy|sell|list|execute|reject|expire|parse
cargo run -- trade task add|list|pause|resume|advise|sim|grid
cargo run -- trade analyze <SYMBOL>             # market state + engine verdict
cargo run -- trade regime <SYMBOL>              # current regime + auto config
cargo run -- trade strategy regimes             # per-regime winners table
cargo run -- trade dex quote|sign|reconcile     # DEX quotes + manual sign
cargo run -- trade history status|refresh       # cached OHLCV ranges
cargo run -- trade lessons / backtests          # learned lessons + recorded runs
cargo run -- trade pause|resume <alloc-id>
```

### Risk Rules (design targets, adjustable within safe ranges)

```
Max position size:      1-25% (default 10%)
Max drawdown lock:      20-80% of entry (default 50%)
Profit lock-in:         1.2x-5x multiplier (default 2x)
Circuit breaker:        5-30% BTC drop in 1h (default 15%)
Wash sale prevention:   1-720h (default 48h)
Max concentration:      10-60% (default 40%)
Max monthly drawdown:   -10% to -50% (default -30%)
```
All ranges verified by: bounds check → consistency → hazard detection → history check

### Decision Quality Gates (design target)

```
Agent Cycle (up to 20 steps):
  Load Context → Delegate Sub-Agents → Hypothesis
  → Self-Critique → MoE → Adversarial Debate
  → Decision Proof → Execute

Sub-Agents run in parallel: Technical, On-Chain,
News, Macro, Risk Auditor, Search, Self-Critique

Every plan: MoE → Debate → Evidence Chain
```

### Transparency & Introspection (design target)

```
Live Brain View    → AI thinking RIGHT NOW
Decision Proof     → Why every trade was made
Stats Dashboard    → Win rate, Sharpe, attribution
Strategy Breakdown → Which strategies work
AI Quality Metrics → Hallucination rate, adherence
```

### Testing Levels

```
Level 1: Unit tests         → Every function, every risk rule, every edge case
Level 2: CLI mock scenarios → 10 synthetic market scenarios, deterministic
Level 3: Historical backtest → Real AI vs real historical data
Level 4: Live shadow mode   → Real market, real AI, virtual trades
```

---

## Vulnerability Assessment

See **[vulnerability-response.md](vulnerability-response.md)** — 7 critical
vulnerabilities identified, analyzed, and mitigated:

| # | Vulnerability | Severity | Status |
|---|---|---|---|
| V1 | API Rate Limiting (429 cascades) | 🟠 HIGH | Mitigated: Tiered execution, API budget, sub-agent batching |
| V2 | Context Window Bloat | 🟠 HIGH | Mitigated: Rolling summaries, memo priority scoring, 5x compression |
| V3 | Debate Paralysis (over-rejection) | 🟡 MEDIUM | Mitigated: Weighted attack scoring, mitigation discount, rate monitoring |
| V4 | Execution Latency vs Volatility | 🟠 HIGH | Mitigated: Staleness guard, per-asset deviation thresholds |
| V5 | Dead Man's Switch (7 days) | 🟡 MEDIUM | Mitigated: Dynamic expiration (2x cycle interval, 4-72h clamp) |
| V6 | Search Agent Data Poisoning | 🟡 MEDIUM | Mitigated: Domain whitelist, multi-source corroboration |
| V7 | Mid-Cycle Crash Desync | 🟠 HIGH | Mitigated: Atomic commit, draft state, write-ahead journal |

### Trading Modes (design target)

```
/trade mode            → View / switch mode

🔒 ACCUMULATE (Buy-Only):  buy dips only, no sells (except emergency)
                          tiered deployment: 30% at -10%, 30% at -15%, 40% at -25%
                          metrics: avg entry discount, cost basis

⚡ SWING (Two-Sided):     full catalog — buy dips, take-profit, trailing,
                          staged exits, stop-loss
                          metrics: win rate, profit factor, realized P&L
```