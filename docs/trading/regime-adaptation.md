# Market-Regime Auto-Adaptation & the AI/Algorithm Division of Labor

> **Status: implemented** (2026-09-07). The bot self-adapts to the current
> market regime without commands. This file explains *how* it picks its
> strategy and — more importantly — *who* decides what in the trading stack.

---

## 1. The core question: who decides?

The trading stack has **two very different brains**. They are not competing;
they are layered.

| Layer | What it owns | What it never does |
|---|---|---|
| **Deterministic engine** (`engine.rs`, `strategies.rs`, `regime.rs`) | Actual trade decisions: buy/sell/hold, lot sizing, exits, no-loss invariant, ATH guard, regime auto-selection | Never "thinks" or hallucinates; no LLM in the loop |
| **AI layer** (`ask.rs`, `debate.rs`, news worker, model ranking) | Context: news summaries, market-state blocks, bull/bear/arbiter debate, `/ask` analysis, tuner proposals | Never executes a trade and never overrides the engine's guardrails |

The rule is one sentence: **the algorithm decides, the AI informs.**

---

## 2. The deterministic engine (owns the money)

Runs on every `tick` / `buy` / `sell` with the current price. Pure arithmetic,
no randomness, no network calls:

1. **Regime classifier** (`regime.rs`) — looks at the last 200 daily closes
   and classifies the market: `DIP` / `EUPHORIA` / `BEAR` / `BULL` / `SIDEWAYS`
   (SMA20/50/200 cross + RSI14 + ATH distance).
2. **Auto config selection** (`auto_engine_config`) — picks the engine config
   for *this* regime:
   - explicit operator choice (`trade strategy apply`, `trade config --set`)
     always wins;
   - else the per-(symbol × regime) winner trained on real history
     (`regime_profiles` table);
   - else the latest applied config;
   - else the runner profile (GoldenCross, tp 1.0, trail 0.30).
3. **Plan + execute** — `plan_buy` / `plan_sell` enforce the invariants:
   - **no-loss**: never sell below `min_exit_price` (entry + fees + margin);
     underwater lots become `COLD_HOLD` (parked, not realized);
   - **ATH guard**: no buying near a recent all-time high;
   - **conviction filter**: optional deep-dip entries (`min_dip_from_high_pct`);
   - **tranches + circuit breakers + operator constraints** (ceiling/floor/
     pause-buy/drawdown).

The engine is *deliberately dumb*: it cannot be talked into a bad trade,
because it has no language model to talk to.

## 3. The AI layer (informs, never executes)

- **News worker** — fetches RSS/API feeds per symbol, dedups by URL, stores
  raw articles (7d) + summary history (30d). The AI prompt only ever sees
  compact verified summaries; raw text is pulled on demand.
- **`/ask` pipeline** — builds context: market-state block (`analyze.rs`:
  SMA/RSI/ATH/S-R/vol/halving phase/seasonality), engine verdict, news
  summaries, user memos, trained-strategy stats, seasonality block. The model
  answers questions and proposes plans — it does not place orders.
- **Debate room** (`debate.rs`) — bull vs bear vs arbiter on a sell decision.
  The verdict is *advisory*; it is persisted to `agent_debates` and printed
  with SELL advice, but the deterministic engine still gates the actual fill.
- **Tuner** (`tuner.rs`) — proposes config changes from recorded lessons; the
  operator applies them explicitly (`trade strategy apply`).

## 4. Why this split works

- **No hallucinated trades**: the LLM cannot touch the ledger. Worst case it
  gives bad advice; the operator sees it and ignores it.
- **No frozen strategy**: the regime classifier + per-regime winners let the
  bot switch behavior (deep-dip buys in DIP, park-cash in BEAR) without any
  command — the "self-adaptation" the operator asked for.
- **Honest metrics**: trade win-rate is 100% *by construction* (no-loss
  invariant). The real numbers are `profitable windows`, `cold-lots`, and
  `max DD` — see the regime sweep in todo.md §13.

## 5. Regime sweep results (real history, 180d windows, step 30d, $200)

| Regime | Windows | Avg PnL | Profitable | Read |
|---|---|---|---|---|
| BULL | 98 | +54.0% | 96% | strong — buy dips, ride trend |
| EUPHORIA | 28 | +58.9% | 100% | strongest — momentum |
| DIP | 47 | +26.6% | 72% | good — deep-dip entries |
| SIDEWAYS | 82 | +1.9% | 45% | flat — cash drag |
| BEAR | 83 | −20.4% | 24% | weak — cold lots mark-to-market |

BEAR losses are *unrealized* (parked COLD lots), not realized losses — the
no-loss invariant never sells below entry. This is exactly why regime
auto-selection matters: the BEAR winner from the next `--force` retrain
should be a park-cash config (tight margins, no dip entries).

## 6. Where the operator fits

The operator is the **final authority**:

- `trade recommend` / `trade task advise` — the bot suggests; the operator
  executes on their own exchange and reports back (`recommend execute` /
  `recommend parse`), and the bot records the outcome and learns.
- `trade dex sign` — the operator signs the swap payload in their wallet UI;
  the bot never holds the private key.
- `trade config --set` / `trade strategy apply` — explicit overrides that
  beat auto-adaptation.