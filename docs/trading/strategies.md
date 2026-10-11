r# Trading Strategies — Safe & Profitable Playbook

> **Role:** Crypto Analyst & Strategy Architect  
> **Principle:** Every strategy must have a verifiable edge, defined risk, and
> a clear reason WHY it should work. No "AI magic" — only testable hypotheses.

---

## The Mission: Opportunity Hunting, Not Schedule Spending

The bot's job is NOT "invest the $100 somehow." It is:

> **Find and enter the best realistic opportunities that actually existed —
> buy at support, ride the recovery, sell at profit — without gambling on
> lottery-tier assets or deploying capital just to deploy it.**

This changes the success metrics fundamentally:

| Wrong metric (lottery DCA) | Right metric (opportunity hunting) |
|---|---|
| "% of capital deployed" | "% of opportunities correctly identified" |
| "bought something every week" | "waited weeks, then struck with precision" |
| "average entry price" | "average entry discount vs 30d market average" |
| "traded a lot" | "profit factor on FEWER, better trades" |

**Cash is a position.** Sitting in USD is not "doing nothing" — it is
reserving ammunition for the moment the market offers a real discount.
The bot that deploys $100 at -12% discounts on blue chips beats the bot
that deployed $100 at -3% discounts on a schedule, every single time.

---

## Strategy Design Philosophy

The AI operates on **"short long-term"** horizon (days to weeks, not minutes
or years). This is the sweet spot where:

- **Noise is filtered out** — 1-minute wicks don't matter
- **Fees are manageable** — 0.1% on a 5% move is 2% cost, acceptable
- **AI reasoning adds value** — macro analysis, news sentiment, on-chain data
  have time to play out
- **Human oversight is possible** — user can review decisions daily, not hourly

### Core Principles

1. **Every position has a thesis.** No "the chart looks bullish." Must cite:
   support/resistance levels, volume confirmation, macro alignment, news catalyst.
2. **Risk is defined before entry.** Stop-loss, position size, and max loss
   are calculated before the trigger is set.
3. **The AI is rewarded for being right about direction AND timing.**
   Profit factor > 1.5 means the system works even if win rate is only 40%.
4. **Cash is a position.** Sitting in USD waiting for a setup IS a valid decision.
   The AI must be comfortable doing nothing.

---

## Strategy Catalog

### Strategy 1: Support Accumulation (Low Risk)

**Thesis:** Buy assets at historically strong support levels during macro
accumulation phases. Hold until mean reversion or trend continuation.

**Entry Conditions (ALL must be true):**
1. Price is within 3% of a support level that held **at least 3 times** in the last 90 days
2. 24h volume is above 30-day average (confirms interest at this level)
3. Macro context is NOT "distribution" or "bear market"
4. Asset is NOT in news for negative catalyst (hack, regulatory, team exit)
5. RSI on daily timeframe is below 40 (oversold territory)

**Position Sizing:**
- Base: 5% of available capital
- If macro = "accumulation": 8%
- If macro = "bull market": 10%
- Never more than 10% regardless

**Exit Strategy:**
- Primary: Take profit at +15% (sell 50%) and +25% (sell remaining 50%)
- Secondary: Trailing stop activates after +10%, trails by 5%
- Hard stop: -15% from entry (never wider)
- Time stop: If not in profit after 14 days, re-evaluate

**Expected Metrics:**
- Win rate: 55-65%
- Average win: +18%
- Average loss: -12%
- Profit factor: 1.8-2.2
- Typical hold: 5-14 days

**Why This Works:** Support levels represent zones where buyers have
historically stepped in. Buying at support with volume confirmation
gives a statistical edge. The 3-touch rule filters out noise levels.

---

### Strategy 2: Trend Continuation (Medium Risk)

**Thesis:** In a confirmed uptrend, buy pullbacks to moving averages.
The trend is your friend until it's broken.

**Entry Conditions (ALL must be true):**
1. Price is above 50-day AND 200-day moving averages (uptrend confirmed)
2. Price has pulled back to within 2% of the 20-day EMA
3. The pullback is on declining volume (not distribution, just profit-taking)
4. 50-day MA is sloping UP (trend is accelerating, not decelerating)
5. BTC correlation is positive (market-wide trend, not isolated pump)

**Position Sizing:**
- Base: 6% of available capital
- If the asset is the sector leader (highest 30-day return): +2%
- If volume on pullback is < 50% of average: +1% (weak selling pressure)

**Exit Strategy:**
- Primary: Trailing stop with 8% trail after +10% gain
- Secondary: Exit if price closes below 50-day MA on daily timeframe
- Hard stop: -10% from entry
- Time stop: 21 days max hold

**Expected Metrics:**
- Win rate: 50-60%
- Average win: +22%
- Average loss: -9%
- Profit factor: 1.6-2.0
- Typical hold: 7-21 days

**Why This Works:** Trending markets tend to continue trending. Pullbacks
to moving averages are high-probability entry points. Declining volume on
pullbacks confirms sellers are exhausted, not accumulating.

---

### Strategy 3: Correlation Divergence (Low/Medium Risk)

**Thesis:** When two highly correlated assets diverge, the underperformer
tends to catch up. Buy the laggard, short (or avoid) the leader.

**Entry Conditions (ALL must be true):**
1. Pair has 30-day correlation > 0.80
2. Asset A outperformed Asset B by > 15% over 7 days
3. The divergence is NOT explained by asset-specific news (e.g., B had a hack)
4. Both assets are in the same macro regime (both bullish or both neutral)
5. Volume on Asset B is stable or increasing (not being abandoned)

**Action:** Buy Asset B (the laggard). Do NOT short Asset A.

**Position Sizing:** 5% of available capital (lower confidence strategy)

**Exit Strategy:**
- Primary: Exit when the 7-day performance gap narrows to < 5%
- Secondary: Take profit at +10%
- Hard stop: -12% from entry
- Time stop: 14 days

**Expected Metrics:**
- Win rate: 45-55%
- Average win: +12%
- Average loss: -10%
- Profit factor: 1.3-1.5
- Typical hold: 5-14 days

**Why This Works:** Correlated assets tend to mean-revert in relative
performance. This is a pairs-trade concept applied directionally (long only
for paper trading v1). The 15% divergence threshold filters noise.

---

### Strategy 4: News Catalyst Fade (High Risk — Experimental)

**Thesis:** Markets overreact to news. Buy the panic, sell the euphoria —
but ONLY when the news doesn't change fundamentals.

**Entry Conditions (ALL must be true):**
1. A significant news event caused a > 8% move in 24 hours
2. The News Sentiment analysis confirms the news is **sentiment, not structural**
   (e.g., "ETF delayed by 2 weeks" vs "protocol hacked and drained")
3. Volume spike is > 3x average (capitulation or euphoria)
4. The move has started to stall (price has not made a new extreme in 4 hours)
5. RSI on 4h timeframe is < 25 (panic sell) or > 75 (euphoria)

**Action:** Fade the move — buy on panic, sell (if holding) on euphoria.

**Position Sizing:** 3% of available capital (HIGH RISK — small size)

**Exit Strategy:**
- Primary: Exit when price retraces 50% of the news-driven move
- Hard stop: -8% from entry
- Time stop: 72 hours (if thesis doesn't play out quickly, news was real)

**Expected Metrics:**
- Win rate: 40-50%
- Average win: +15%
- Average loss: -7%
- Profit factor: 1.2-1.5
- Typical hold: 1-3 days

**Why This Works:** Markets systematically overreact to non-structural news.
The key is distinguishing "sentiment noise" from "fundamental change" —
this is where the News Sentiment AI sub-agent adds value.

---

### Strategy 5: DCA with Conditions (Lowest Risk)

**Thesis:** Accumulate core positions over time, but ONLY when conditions
are favorable. Not blind DCA — intelligent DCA.

**Entry Conditions:**
1. A scheduled DCA event triggers (e.g., every Monday)
2. IF price is below 30-day average: execute full DCA amount
3. IF price is 5-15% above 30-day average: execute 50% DCA amount
4. IF price is > 15% above 30-day average: SKIP this cycle, accumulate cash
5. IF macro = "bear market": reduce all amounts by 50%

**Position Sizing:** Fixed USD amount per cycle (e.g., $50/week)

**Exit Strategy:**
- This is accumulation-only. No sell triggers from this strategy.
- Other strategies (Support Accumulation, Trend Continuation) handle exits.
- Maximum accumulation per asset: 40% of portfolio (concentration limit).

**Expected Metrics:**
- Not measured on P&L (accumulation strategy)
- Success metric: average entry price < 30-day average price (buying dips)

**Why This Works:** This removes the biggest DCA weakness — buying
regardless of price. The conditions ensure capital is deployed when
it's most effective.

### Strategy 6: Conviction Dip-Buying (Implemented — `Conviction-Dip-Buyer`)

The implemented counterpart of "cash is a position": the engine refuses to
buy unless price is `min_dip_from_high_pct` (default 40%) below the recent
high, then lets winners run to a large take-profit (100–200%) with a wide
trailing stop (0.30–0.50). Fewer trades, each significant.

- CLI: `trade strategy apply Conviction-Dip-Buyer`; training grid
  `trade strategy train --grid-conviction` (54 configs: dips 15–50% ×
  3 tp/trail pairs × 3 profit modes — 15/20/25% are BTC-friendly).
- Backtest/batch: `--min-dip P` flag + inline config override.
- Bench result (365d windows, step 30d, $200, 4 symbols): conviction beats
  the runner profile on ETH (+22.8% vs +13.7%), SOL (+25.6% vs +9.2%),
  LINK (+28.8% vs +20.9%) with 2–8× fewer buys and 6–35× higher profit per
  trade. BTC is slightly worse (−1.3pp) — BTC dips are shallower, so the
  30%+ dip entries rarely trigger.

---

## Strategy Selection Logic

The AI doesn't pick a strategy randomly. It evaluates which strategy
matches current conditions:

```
┌─────────────────────────────────────────────────────┐
│              STRATEGY SELECTOR                       │
├─────────────────────────────────────────────────────┤
│                                                      │
│  Is macro = BULL?                                    │
│    ├─ YES → Is price above 50 & 200 MA?              │
│    │         ├─ YES → Is there a pullback to 20 EMA? │
│    │         │         ├─ YES → STRATEGY 2 (Trend)   │
│    │         │         └─ NO  → Wait in USD          │
│    │         └─ NO  → Is price at support?           │
│    │                   ├─ YES → STRATEGY 1 (Support) │
│    │                   └─ NO  → Wait in USD          │
│    │                                                  │
│    └─ NO (accumulation / neutral / bear) →           │
│         Is price at support with volume?              │
│           ├─ YES → STRATEGY 1 (Support)              │
│           └─ NO  → Any correlation divergence >15%?  │
│                     ├─ YES → STRATEGY 3 (Correlation)│
│                     └─ NO  → Is there a news overreaction?│
│                               ├─ YES → STRATEGY 4 (Fade) │
│                               └─ NO  → STRATEGY 5 (DCA)  │
│                                                      │
└─────────────────────────────────────────────────────┘
```

---

## Strategy Performance Tracking

Every trade is tagged with the strategy that generated it. This enables
strategy-level performance analysis:

| Strategy | Trades | Win Rate | Avg Win | Avg Loss | Profit Factor | Status |
|---|---|---|---|---|---|---|
| Support Accumulation | 45 | 62% | +18.2% | -11.5% | 2.15 | ✅ Active |
| Trend Continuation | 28 | 54% | +22.1% | -8.9% | 1.82 | ✅ Active |
| Correlation Divergence | 12 | 42% | +11.8% | -9.5% | 1.08 | ⚠️ Monitor |
| News Fade | 8 | 38% | +14.5% | -6.8% | 1.12 | 🧪 Experimental |
| DCA (Conditions) | 20 | N/A | N/A | N/A | N/A | ✅ Active |

Strategies with profit factor < 1.2 for > 20 trades are flagged for review.
Experimental strategies are capped at 5% of total capital deployed.

---

## Anti-Strategy: What the AI Must NEVER Do

| Forbidden Action | Reason |
|---|---|
| Buy because "it's going up" | Chasing momentum without a defined entry zone |
| Sell because "it's going down" | Panic selling without evaluating the thesis |
| Double down on a losing position | "Averaging down" turns a small loss into a big loss |
| Trade without a stop-loss | Every position must have a defined max loss |
| Trade more than 3 assets simultaneously | Cognitive overload; dilution of thesis quality |
| Trade during low liquidity (weekends, holidays) | Wide spreads, unpredictable price action |
| Trade based on a single data point | Requires multiple confirming signals |
| Change strategy mid-trade | Thesis drift; exit and re-enter if strategy changed |
| Ignore correlation | Holding BTC + ETH + SOL = 3x same direction risk |
| Trade illiquid assets (< $10M daily volume) | Slippage and manipulation risk |

---

## Trading Modes (User-Controlled)

The bot operates in one of two modes, switchable at any time via `/trade mode`.
The mode is injected into every AI prompt and enforced by the Risk Manager
(neither the AI nor a prompt can override it).

| | 🔒 ACCUMULATE (Buy-Only) | ⚡ SWING (Two-Sided) |
|---|---|---|
| **Goal** | Build a position at the best prices | Buy low, sell high, repeat |
| **Buys** | ✅ At discounts, support, macro dips | ✅ Same, full strategy catalog |
| **Sells** | ❌ Only emergency / thesis broken | ✅ Take-profit, trailing, stages |
| **Typical user** | "Give bot $100, accumulate BTC/ETH/SOL/LINK" | "Let bot trade both directions" |
| **Risk profile** | Lowest (never exits, no timing risk on exit) | Moderate (exit timing matters) |
| **Key metric** | Average entry discount vs market, cost basis | Win rate, profit factor, realized P&L |

### Mode 1: Accumulate (Buy-Only)

**Philosophy:** The user gives the bot capital and says "buy the best dips."
The bot deploys capital ONLY on strong discount setups and never sells —
it accumulates. This is "DCA on minimums": same assets, but entries only
at the best prices, never at random.

**Allowed triggers:**
- ✅ `Buy` triggers (PriceBelow / support zones / macro dips)
- ✅ `TimeDecay` (cleanup of stale buy triggers)
- ❌ `Sell` triggers (TakeProfit, TrailingStop, StopLoss) — REJECTED by Risk Manager
- ⚠️ Exception: **thesis invalidation** — if a blue chip shows a fundamental
  break (regulatory ban, chain hack, delisting), the bot MAY sell to protect
  capital. User-approved, configurable (`allow_emergency_sells`).

**Sizing logic (important):** In Accumulate mode the bot must NOT spend all
capital quickly. It should keep dry powder for better discounts:
- First 30% of capital: spend only on -10%+ discounts (deep)
- Next 30%: only on -15%+ discounts (deeper)
- Last 40%: reserved for extreme -25%+ opportunities or macro capitulation
- This is **tiered deployment**, not schedule-based

**Success metrics:**
- Average entry discount vs 30-day average price (target: better than -8%)
- Cost basis relative to portfolio value (lower = better)
- "Regret index": how much better could entries have been (vs perfect oracle)

**User-facing example:**
```
🔒 ACCUMULATE MODE — Day 30
Capital: $100.00 | Deployed: $62.00 | Reserved: $38.00

Entries:
  BTC: $58,000 (-12.3% vs 30d avg)  $25.00
  ETH: $2,410  (-9.8% vs 30d avg)   $22.00
  SOL: $118    (-14.2% vs 30d avg)  $15.00

Average entry discount: -12.1% 🎯 (target -8%)
Cost basis vs current value: +4.2% unrealized

Waiting for deeper discounts to deploy remaining $38.00
[⚡ Switch to Swing] [📊 Details]
```

### Mode 2: Swing (Two-Sided)

**Philosophy:** Full trading loop — enter at discounts, exit at targets,
reuse capital. This is the complete strategy catalog from this document.

**Allowed triggers:**
- ✅ `Buy` (PriceBelow, support, macro dips)
- ✅ `Sell` (TakeProfit stages, TrailingStop, StopLoss)
- ✅ `TimeDecay` cleanup

**Key difference from Accumulate:** Capital is NOT reserved for later — it
is recycled through win → exit → new entry. Position sizing per the strategy
catalog (5-10%), exits per strategy rules.

**Success metrics:** Win rate, profit factor, realized P&L, capital efficiency
(turnover).

### Switching Modes

```
/trade mode          → Shows current mode + inline buttons
/trade mode swing    → Switch to two-sided
/trade mode accumulate → Switch to buy-only
```

**Switch safety rules:**
- Switching Accumulate → Swing: instant. Existing buy triggers remain valid.
- Switching Swing → Accumulate: pending SELL triggers are cancelled (with a
  confirmation prompt), BUY triggers remain.
- Mode changes are logged in the audit trail with user ID and timestamp.
- Mode is injected into the AI system prompt: "You are in ACCUMULATE mode.
  You may only set Buy triggers. Do not propose sells (Risk Manager will
  reject them)."
