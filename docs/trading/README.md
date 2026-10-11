# Trading Module — Documentation Index

> **Branch:** `master`
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
[`TRADING_MODULE_PLAN.md`](TRADING_MODULE_PLAN.md). The CLI is the 1:1 testing
surface for every trading command; see the
the repository test suite (`cargo test`) for verified behavior.

---

## Document Map

| Document | Description | Status |
|---|---|---|
| [`TRADING_MODULE_PLAN.md`](TRADING_MODULE_PLAN.md) | Canonical engineering plan: architecture, schema, invariants, command matrix, backtesting, testing | ✅ current |
| [`architecture.md`](architecture.md) | High-level system architecture, component diagram, data flow | ⏳ design |
| [`data-models.md`](data-models.md) | All data structures: VirtualPortfolio, ActiveTrigger, VirtualTrade, TradingJournal | ⏳ design |
| [`execution-engine.md`](execution-engine.md) | Rust algorithmic engine: trigger evaluation, trade execution, consensus pricing | ⏳ design |
| [`risk-management.md`](risk-management.md) | Hardcoded risk rules + safe ranges + control levels + integrity verification + hazard detection | ⏳ design |
| [`ai-tools.md`](ai-tools.md) | AI Function Calling API: all 11 tools available to the AI strategist | ⏳ design |
| [`moe-pipeline.md`](moe-pipeline.md) | MoE double-checking + Adversarial Debate (AI vs AI battle) + Decision Proofs with evidence chain | ⏳ design |
| [`fee-accounting.md`](fee-accounting.md) | Fee simulation, slippage modeling, gas estimation, real P&L calculation | ⏳ design |
| [`user-commands.md`](user-commands.md) | Telegram `/trade` + `/tradetask` mirrors, CLI 1:1 surface | ✅ implemented |
| [`dex-execution.md`](dex-execution.md) | Jupiter/0x quotes, manual sign, fill reconciliation, Belarus P2P on-ramp | ✅ implemented |
| [`regime-adaptation.md`](regime-adaptation.md) | Market-regime classifier, auto config selection, AI vs algorithm roles | ✅ implemented |
| [`vulnerability-response.md`](vulnerability-response.md) | 7 critical vulnerabilities + mitigations | ✅ current |
| [`logging-analytics.md`](logging-analytics.md) | Log export, performance analytics, decision attribution, file formats | ⏳ design |
| [`model-training.md`](model-training.md) | AI learning system: performance review, strategy evolution, A/B testing | ⏳ design |
| [`strategies.md`](strategies.md) | **CRYPTO ANALYST:** 5 safe strategies with concrete entry/exit rules, expected metrics, anti-patterns | ⏳ design |
| [`agent-architecture.md`](agent-architecture.md) | **ARCHITECT:** Multi-step agent thinking (≤20 steps), sub-agent delegation, search, budget | ⏳ design |
| [`introspection.md`](introspection.md) | **TRANSPARENCY:** Live brain view, decision proofs, stats dashboard, model attribution | ⏳ design |
| [`stability.md`](stability.md) | **RELIABILITY:** Fault tolerance, crash recovery, graceful degradation, predictability contract | ⏳ design |
| [`testing.md`](testing.md) | Full testing: unit tests, CLI mock scenarios, historical backtest, live shadow, fuzzing | ⏳ design |
| [`security.md`](security.md) | Security measures, dead man's switch, audit trail, emergency stop | ⏳ design |

> "⏳ design" documents describe the *target* paper-trading sandbox. The parts
> already shipped (CLI engine, invariants, backtests, scenarios) are documented
> in [`TRADING_MODULE_PLAN.md`](TRADING_MODULE_PLAN.md).

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