# AI Introspection & Transparency

> **Role:** System Architect  
> **Principle:** The AI is not a black box. Every thought, every tool call,
> every rejected hypothesis is visible, searchable, and auditable.

---

## What "Seeing Inside the Brain" Means

At any moment, the user can answer these questions:

| Question | Where to Look |
|---|---|
| What is the AI thinking RIGHT NOW? | Live Brain View (during active cycle) |
| Why did the AI make that trade? | Cycle History → Decision Proof |
| What did the AI consider but reject? | Decision Tree → Alternative Hypotheses |
| How confident was the AI? | Confidence score + debate results |
| What data did the AI base this on? | Evidence Chain → Data Sources |
| Has the AI been improving? | Performance Dashboard → Trend charts |
| Which strategy is winning? | Strategy Performance table |
| Is the AI following my instructions? | Strategy Adherence score |
| What is the AI planning next? | Active Triggers + Pending Analysis |
| How often does the AI hallucinate? | Hallucination Rate metric |

---

## Live Brain View

### During an Active Cycle

```
🧠 AI BRAIN — LIVE
Cycle #142 | Running: 8.3s | Step 9/20 | Budget: 11 left

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

✅ Step 1  read_memos()                       0.1s
✅ Step 2  get_macro_context()                0.3s
✅ Step 3  get_portfolio_state()              0.2s
✅ Step 4  read_market_snapshot([BTC,ETH,SOL]) 0.5s
✅ Step 5  get_news_sentiment("SOL")          0.4s
✅ Step 6  Internal analysis: no warnings     0.8s
🔄 Step 7  → Technical Analyst (SOL daily)    2.1s
🔄 Step 8  → On-Chain Analyst (SOL)           1.9s
⏳ Step 9  → Search Agent ("SOL ecosystem")   running...

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Current Hypothesis: "BUY SOL at $130 support"
Confidence so far: 65% (pending sub-agent results)

Sub-agents running: 3 | Completed: 0
Search budget: 2/3 remaining

[🔄 Refresh] [⏹️ Cancel Cycle] [📋 Full Log]
```

### Between Cycles (Idle)

```
🧠 AI BRAIN — IDLE
Last cycle: #142 | Aug 5, 2026 14:00 UTC | Duration: 12.3s

Next cycle: Aug 5, 2026 20:00 UTC (in 5h 52m)
Or triggered by: trade execution event

Active Triggers: 3
Pending Analysis: None

Last Decision: BUY SOL at $130 ($50)
Confidence: 78% | Debate: 5/7 defended
Status: Waiting for price to reach $130

[🔄 Force Cycle Now] [📋 Last Cycle Details] [📊 Stats]
```

---

## Decision Proof (Why Did the AI Do That?)

Every decision comes with a full evidence package. This is what the user sees
when they click on any trade or trigger in the history:

```
═══════════════════════════════════════════════════
  DECISION PROOF #D142-3
  Action: BUY SOL at $130.00 ($50.00)
  Cycle: #142 | Aug 5, 2026 14:00 UTC
  Confidence: 78% 🟢 HIGH
═══════════════════════════════════════════════════

📊 MARKET CONTEXT (at decision time)
  SOL Price:    $142.30
  24h Change:   -2.1%
  7d Change:    +3.5%
  30d Change:   +8.2%
  ATH Discount: -45.3%
  24h Volume:   $3.2B

📈 TECHNICAL ANALYSIS
  Support $130: 4 touches in 90 days ✓
  RSI (daily):  38.5 — approaching oversold ✓
  Divergence:   Bullish (price ↓, RSI ↑) ✓
  Volume:       Above 30d average at support zone ✓
  Patterns:     None bearish ✓

🔗 ON-CHAIN DATA
  Exchange outflows: -$45M SOL this week (accumulation)
  Whale movements:   No large sells detected
  Staking:           72% of supply staked (stable)

📰 NEWS & CATALYSTS
  Sentiment:    Neutral-Bullish
  Key Event:    Solana Breakpoint Conference Sep 15-17
  Historical:   SOL rallied 15-25% before past Breakpoints
  Risk:         No negative catalysts detected

⚠️ RISK ASSESSMENT
  Max loss:     -$7.50 (15% stop on $50 position)
  Portfolio %:  4.9% of total (limit: 10%) ✓
  Correlation:  SOL-BTC 0.78 — moderate risk ⚠️
  Concentration: SOL would be 22% of portfolio (limit: 40%) ✓

🔄 REJECTED ALTERNATIVES
  ✗ BUY at $125: Weak support (1 touch). May not fill.
  ✗ BUY at $140 breakout: Chasing. Lower R:R.
  ✗ DO NOTHING: Opportunity cost too high.

🗣️ ADVERSARIAL DEBATE
  Challenger attacks: 7 | Defender wins: 5
  Key attack: "SOL-BTC correlation 0.78. If BTC drops,
              SOL drops more." → Position reduced $60→$50.
  Verdict: APPROVED

✅ RISK VALIDATION
  Position size:  $50 (5.0%) ≤ $100 (10%) ✓
  Drawdown lock:  N/A (new position) ✓
  Concentration:  22% ≤ 40% ✓
  Wash sale:      No SOL sold in 48h ✓
  Liquidity:      $3.2B ≥ $500 (10x $50) ✓
  Slippage est.:  0.0002% ≤ 2% ✓

═══════════════════════════════════════════════════
[📋 Raw JSON] [📊 Related Trades] [🔄 Replay Decision]
```

---

## Statistics & Performance Dashboard

### Portfolio Dashboard (`/trade`)

```
┌──────────────────────────────────────────────────┐
│            📊 TRADING DASHBOARD                    │
│            Aug 5, 2026 14:00 UTC                   │
├──────────────────────────────────────────────────┤
│                                                    │
│  Portfolio Value:  $1,023.40                       │
│  Total Return:     +2.34%  📈                      │
│  Available USD:    $650.00                         │
│  Locked Capital:   $0.00                           │
│                                                    │
│  ┌─────────────────────────────────────────────┐  │
│  │  Portfolio Composition                       │  │
│  │  USD ████████████████████░░░░░ 63.5%        │  │
│  │  BTC ██████░░░░░░░░░░░░░░░░░░ 16.0%        │  │
│  │  SOL ████████░░░░░░░░░░░░░░░░ 20.5%        │  │
│  └─────────────────────────────────────────────┘  │
│                                                    │
│  Active Triggers: 3                                │
│  Today's Trades:  0                                │
│  AI Cycles Run:   142                              │
│  Last AI Activity: 12 minutes ago                  │
│                                                    │
└──────────────────────────────────────────────────┘
```

### Performance Statistics (`/trade performance`)

```
┌──────────────────────────────────────────────────┐
│            📈 PERFORMANCE STATISTICS               │
├──────────────────────────────────────────────────┤
│                                                    │
│  RETURNS                                           │
│  ┌─────────────────────────────────────────────┐  │
│  │  Total Return:     +2.34%                    │  │
│  │  Realized P&L:     +$18.30                   │  │
│  │  Unrealized P&L:   +$5.10                    │  │
│  │  Max Drawdown:     -4.2% (Jul 28)            │  │
│  │  Sharpe Ratio:     1.45                      │  │
│  │  Sortino Ratio:    2.10                      │  │
│  └─────────────────────────────────────────────┘  │
│                                                    │
│  TRADING ACTIVITY (All Time)                       │
│  ┌─────────────────────────────────────────────┐  │
│  │  Total Trades:     18                        │  │
│  │  Winning:          11 (61.1%)                 │  │
│  │  Losing:           7 (38.9%)                  │  │
│  │  Profit Factor:    1.85                      │  │
│  │  Avg Win:          +$12.40 (18.3%)           │  │
│  │  Avg Loss:         -$8.20 (-11.5%)           │  │
│  │  Avg Hold Time:    6.2 days                  │  │
│  │  Total Fees:       -$3.60                    │  │
│  │  Total Slippage:   -$1.80                    │  │
│  └─────────────────────────────────────────────┘  │
│                                                    │
└──────────────────────────────────────────────────┘
```

### Strategy Breakdown

```
┌──────────────────────────────────────────────────┐
│            🎯 STRATEGY PERFORMANCE                 │
├──────────────────────────────────────────────────┤
│                                                    │
│  Strategy          Trades  Win%   P.Factor  P&L   │
│  ──────────────────────────────────────────────── │
│  Support Accum.    8       75%    2.40     +$28   │
│  Trend Contin.     5       60%    1.65     +$12   │
│  Correlation Div.  3       33%    0.85     -$8    │
│  News Fade         2       50%    1.20     +$3    │
│  DCA (Condition)   N/A     N/A    N/A       N/A   │
│                                                    │
│  ⚠️ Correlation Div: flagged for review (P.F. <1) │
│                                                    │
└──────────────────────────────────────────────────┘
```

### AI Quality Metrics

```
┌──────────────────────────────────────────────────┐
│            🤖 AI QUALITY METRICS                   │
├──────────────────────────────────────────────────┤
│                                                    │
│  Cycles Run:              142                      │
│  Triggers Set:            95                       │
│  Triggers Executed:       28 (29.5% fill rate)     │
│  Triggers Cancelled:      42                       │
│  Triggers Expired:        25                       │
│                                                    │
│  Hallucination Rate:      0.7% (1 of 142 cycles)  │
│  Risk Vetos:              8 (5.6% of triggers)     │
│  Self-Corrections:        3 (2.1% of cycles)       │
│                                                    │
│  Debate Statistics:                                │
│    Plans Approved:        118 (83.1%)              │
│    Plans Modified:        18 (12.7%)               │
│    Plans Rejected:        6 (4.2%)                 │
│    Avg Confidence:        0.76                     │
│                                                    │
│  Strategy Adherence:      92% 🟢                   │
│  (How well AI follows your directives)             │
│                                                    │
└──────────────────────────────────────────────────┘
```

### AI Model Performance

```
┌──────────────────────────────────────────────────┐
│            🔬 MODEL ATTRIBUTION                    │
│                                                    │
│  Which AI models produce the best decisions?       │
│                                                    │
│  Generator Model       Trades  Avg P&L  Win Rate  │
│  ──────────────────────────────────────────────── │
│  qwen-2.5-72b (groq)   12     +$14.20   66.7%    │
│  llama-3.1-70b (grov)  10     +$8.50    60.0%    │
│  deepseek-v3 (openrt)  6      +$5.30    50.0%    │
│                                                    │
│  Critic Model          Cycles  Halluc.Caught      │
│  ──────────────────────────────────────────────── │
│  gemini-2.5-pro         95     3                   │
│  claude-3.5-sonnet      47     1                   │
│                                                    │
│  Arbiter Model          Debates  Rejections       │
│  ──────────────────────────────────────────────── │
│  gemini-2.5-pro         142     6 (4.2%)          │
│                                                    │
└──────────────────────────────────────────────────┘
```

---

## Real-Time Notifications

### Trade Execution Alert

```
🔔 TRADE EXECUTED
Aug 5, 2026 14:23 UTC

BUY 0.3846 SOL @ $130.00
Amount: $50.00 | Fee: $0.05 | Slippage: $0.01

Trigger set: Aug 5 14:00 (23 min ago)
AI Reasoning: "Support bounce + Breakpoint catalyst"
Confidence: 78%

Portfolio: $973.34 (USD) + 0.3846 SOL ($50.00)
Total: $1,023.34

[📝 View Decision Proof] [❌ Emergency Stop]
```

### AI Cycle Completed

```
🧠 AI CYCLE COMPLETED
Cycle #143 | Aug 5, 2026 20:00 UTC
Duration: 14.2s | Steps: 18/20

Summary:
• Held SOL position (up 2.1% from entry, thesis intact)
• Cancelled ETH buy trigger (price moved away)
• Set BTC buy trigger at $58,000 ($80)
• No new concerns

Confidence: 82% | Debate: 6/7 defended

[📋 Full Cycle Log] [📊 Stats]
```

---

## Transparency Guarantees

1. **Every AI thought is logged.** No hidden reasoning. The full step-by-step
   trace is available for every cycle, forever.

2. **Every decision has a proof.** Why the AI did what it did, what data it
   used, what alternatives it rejected.

3. **Every rejection is explained.** If the Critic rejects a Generator's plan,
   you see exactly why. If the Risk Manager vetoes a trade, you see which rule.

4. **Every parameter change is audited.** Who changed what, when, from what
   value to what value, and what tier it moved to.

5. **Statistics never lie.** All performance metrics are calculated from the
   immutable trade log. No cherry-picking, no survivorship bias.

6. **You can replay any decision.** Feed the same inputs back to the AI at
   any time to see what it would decide today vs. what it decided then.
