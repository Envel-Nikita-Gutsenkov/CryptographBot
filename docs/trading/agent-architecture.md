# Agent Architecture — Multi-Step AI Thinking

> **Role:** System Architect  
> **Core Idea:** The AI is not a single prompt→response. It's an **agent**
> that thinks in steps, delegates to sub-agents, searches for information,
> questions its own assumptions, and iteratively refines its plan.

---

## Agent Loop (Up to 20 Steps)

Each AI trading cycle is not one API call — it's a **reasoning loop** of up
to 20 steps. The AI decides at each step what to do next.

```
┌─────────────────────────────────────────────────────┐
│                  AGENT CYCLE                         │
│                  (every 4-12h)                       │
├─────────────────────────────────────────────────────┤
│                                                      │
│  STEP 0:  Wake up. Load context.                     │
│  STEP 1:  Read memos from past cycles.               │
│  STEP 2:  Get macro context.                         │
│  STEP 3:  Get portfolio state.                       │
│  STEP 4:  Get market snapshot for tracked assets.    │
│  STEP 5:  Get news sentiment for holdings.           │
│  STEP 6:  Analyze: any positions in danger?          │
│  STEP 7:  Delegate to Technical Analyst sub-agent.   │
│  STEP 8:  Delegate to On-Chain Analyst sub-agent.    │
│  STEP 9:  If unclear → Delegate to Search Agent.     │
│ STEP 10:  Synthesize all sub-agent reports.          │
│ STEP 11:  Form initial trading hypothesis.           │
│ STEP 12:  Run risk pre-check on hypothesis.          │
│ STEP 13:  Question assumptions (self-critique).      │
│ STEP 14:  Refine hypothesis based on self-critique.  │
│ STEP 15:  Generate specific triggers.                │
│ STEP 16:  Run MoE validation (Generators + Critic).  │
│ STEP 17:  Run Adversarial Debate.                    │
│ STEP 18:  Build Decision Proof.                      │
│ STEP 19:  Write memos for next cycle.                │
│ STEP 20:  Execute: set/cancel triggers.              │
│                                                      │
│  MAX: 20 steps. After that → forced decision.        │
│                                                      │
└─────────────────────────────────────────────────────┘
```

### Step Budget

The AI has a **step budget** of 20 steps per cycle. Each tool call, sub-agent
delegation, or analysis phase consumes 1 step. The AI must manage its budget —
it cannot call tools infinitely.

```
Remaining steps: 15 → AI is comfortable, can explore
Remaining steps: 5  → AI must focus on critical decisions
Remaining steps: 1  → AI MUST produce final output NOW
```

If the AI exceeds 20 steps without producing a decision, the system forces
a "fast path": skip debate, use last valid hypothesis, log a warning.

---

## Sub-Agent System

The AI can delegate analysis to specialized sub-agents. These are separate
AI calls with focused prompts and limited scope.

### Sub-Agent Catalog

| Sub-Agent | Role | Tools | Max Steps |
|---|---|---|---|
| **Technical Analyst** | Chart patterns, support/resistance, indicators | Market snapshot, price history | 5 |
| **On-Chain Analyst** | Whale movements, exchange flows, staking data | On-chain data APIs | 5 |
| **News Sentiment Analyst** | News interpretation, sentiment scoring | News DB, web search | 5 |
| **Macro Analyst** | Market cycle stage, global liquidity | Global metrics, BTC dominance | 3 |
| **Risk Auditor** | Position sizing, correlation check, stress test | Portfolio state, risk params | 3 |
| **Search Agent** | Web search for current events | Tavily/Exa/DuckDuckGo | 5 |
| **Self-Critique Agent** | Attacks the current plan, finds weaknesses | Current hypothesis, market data | 5 |

### Delegation Protocol

When the main AI delegates to a sub-agent:

```
Main AI: "Technical Analyst, analyze SOL on the daily timeframe.
          Key questions:
          1. Is $130 a valid support level? How many touches?
          2. Is there a bullish divergence on RSI?
          3. What is the volume profile at current levels?
          4. Are there any bearish patterns forming?
          Budget: 5 steps. Return structured analysis."

Technical Analyst (5 steps):
  Step 1: Get SOL price history (90 days)
  Step 2: Identify support/resistance levels
  Step 3: Calculate RSI, check divergences
  Step 4: Analyze volume at key levels
  Step 5: Return structured report

→ Returns to Main AI:
  {
    "support_levels": [
      {"price": 130, "touches": 4, "strength": "strong"},
      {"price": 118, "touches": 2, "strength": "moderate"}
    ],
    "rsi_14d": 38.5,
    "bullish_divergence": true,
    "volume_at_support": "above_average",
    "bearish_patterns": "none",
    "recommendation": "BUY at $130 support with high confidence"
  }
```

### Parallel Delegation

Sub-agents that don't depend on each other run in **parallel**:

```
Step 7:  Delegate to Technical Analyst  ─┐
Step 8:  Delegate to On-Chain Analyst   ─┤ All run simultaneously
Step 9:  Delegate to News Analyst       ─┤
Step 10: Delegate to Macro Analyst      ─┘

Step 11: Collect all results, synthesize
```

This makes the agent cycle fast despite multiple analyses.

---

## Search Agent Integration

When the AI encounters something it doesn't know (e.g., "Is there an ETH ETF
decision this week?"), it delegates to the Search Agent:

```
Main AI: "Search Agent, find:
          1. Any scheduled ETH ETF decisions in the next 7 days
          2. Current market expectations (approval probability)
          3. Recent SEC statements on crypto ETFs
          Budget: 5 steps. Return facts with sources."

Search Agent:
  Step 1: search("ETH ETF decision schedule August 2026")
  Step 2: search("SEC Ethereum ETF approval probability")
  Step 3: search("SEC crypto ETF statements recent")
  Step 4: Synthesize findings with source URLs
  Step 5: Return structured report

→ Returns:
  {
    "events": [
      {"date": "2026-08-12", "event": "SEC deadline for VanEck ETH ETF",
       "probability": "65% per Polymarket", "source": "..."}
    ],
    "sentiment": "cautiously optimistic",
    "key_fact": "SEC has not appealed the ETH commodity classification"
  }
```

The Search Agent reuses the existing search infrastructure (Tavily → Exa →
DuckDuckGo fallback chain, already partially implemented in `src/crypto/ai/search/`).

---

## Chain-of-Thought Visibility

EVERY step the AI takes is logged and visible to the user. This is not a
black box — it's a glass box.

### Live Brain View (Telegram)

When the user clicks "🧠 AI Brain" in the Trading Panel during an active cycle:

```
🧠 AI BRAIN — Cycle #142
Running... Step 9 of 20 | Budget: 11 remaining

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

✅ Step 1: read_memos() → 3 memos loaded
✅ Step 2: get_macro_context() → "Accumulation phase"
✅ Step 3: get_portfolio_state() → $1,023.40 (+2.3%)
✅ Step 4: read_market_snapshot([BTC,ETH,SOL]) → 3 quotes
✅ Step 5: get_news_sentiment("SOL") → "Neutral-Bullish"
✅ Step 6: Analysis → "SOL position up 5.5%, no action needed"
🔄 Step 7: Delegated to Technical Analyst (SOL daily)
🔄 Step 8: Delegated to On-Chain Analyst (SOL)
⏳ Step 9: Delegated to Search Agent ("SOL ecosystem news")

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

[🔄 Refresh]  [⏹️ Cancel Cycle]
```

### Cycle History View

After a cycle completes, the user can review every step:

```
📋 CYCLE #142 — Aug 5, 2026 14:00 UTC
Duration: 12.3 seconds | Steps: 19/20

STEP 1: read_memos() → OK (0.1s)
  Result: 3 memos loaded. Key: "Wait 24h after 5%+ drops"

STEP 6: Analysis → Decision to focus on SOL (0.8s)
  Reasoning: "ETH dropped 6.1% today — skip per lesson.
              SOL at $142, approaching strong $130 support.
              Will delegate for deeper analysis."

STEP 7: Technical Analyst (SOL) → OK (2.1s)
  Key finding: "$130 support held 4 times in 90 days. RSI 38.5
               with bullish divergence forming. Volume above
               average at support zone. No bearish patterns."

STEP 9: Search Agent → OK (3.2s)
  Key finding: "Solana Breakpoint conference announced for Sep 15-17.
                Historically, SOL rallies 15-25% into Breakpoint."

STEP 11: Synthesis (1.5s)
  "Converging signals: technical support ($130), on-chain
   accumulation (exchange outflows -$45M this week), and
   upcoming catalyst (Breakpoint Sep). High conviction BUY."

STEP 16: MoE Validation → APPROVED (2.1s)
  Generator-A: "BUY SOL at $130, $50, take-profit at $156 (+20%)"
  Generator-B: "BUY SOL at $132, $60, take-profit at $150 (+13.6%)"
  Critic: "Synthesized: BUY at $130, $50, staged TP at $150/$160"

STEP 17: Adversarial Debate → APPROVED (1.8s)
  Challenger won 2/7. Attacks: "SOL-BTC correlation is 0.78,
  if BTC drops, SOL drops more." Defender conceded, reduced
  position from $60 to $50.

STEP 19: write_memo() → OK (0.2s)
  "SOL: Entered at $130. Thesis: support bounce + Breakpoint
   catalyst. Targets: $150 (50%), $160 (50%). Max loss: $117."
```

---

## Decision Tree & Alternative Paths

For every decision, the AI records not just what it chose, but what it
considered and rejected:

```
DECISION TREE — SOL Entry

├─ Hypothesis A: BUY at $130 support [SELECTED]
│   Confidence: 78%
│   Basis: 4 touches, RSI divergence, Breakpoint catalyst
│
├─ Hypothesis B: BUY at $125 (deeper support) [REJECTED]
│   Confidence: 35%
│   Reason: $125 only touched once. Weak support. May not reach.
│
├─ Hypothesis C: WAIT for $140 breakout confirmation [REJECTED]
│   Confidence: 45%
│   Reason: Would miss the support entry. Chasing, not anticipating.
│
└─ Hypothesis D: DO NOTHING [REJECTED]
    Confidence: 30%
    Reason: Opportunity cost. Conditions are favorable.
```

This is logged in the Decision Proof and visible to the user.

---

## Self-Correction During a Cycle

The AI can change its mind mid-cycle if new information contradicts its
initial hypothesis:

```
Step 10: Initial hypothesis: "BUY SOL at $130 support"
Step 13: Self-critique: "Wait — has SOL volume been declining?"
Step 14: Check: get SOL volume trend (30 days)
Step 15: Finding: "SOL volume -35% over 30 days. Declining interest."
Step 16: REVISED: "BUY SOL at $125 deeper support, reduce to $30"
```

The system MUST log when the AI self-corrects — this is a sign of good
reasoning, not a bug. The Performance Reviewer tracks self-correction
frequency as a quality metric.

---

## Step Timeouts & Circuit Breakers

Each step has a timeout:

| Step Type | Timeout | On Timeout |
|---|---|---|
| Tool call (local) | 2 seconds | Skip, try alternative |
| Sub-agent delegation | 15 seconds | Skip this sub-agent, proceed without |
| Search Agent | 20 seconds | Skip search, use cached data |
| AI reasoning (internal) | 10 seconds | Force fast-path decision |
| Total cycle | 120 seconds | Force output with best hypothesis so far |

If any step times out, the AI proceeds with degraded information —
it doesn't crash. The cycle log shows which steps timed out.

---

## Predictability & Determinism

While AI is inherently non-deterministic, the system provides:

1. **Deterministic replay:** Every cycle can be replayed with the same inputs.
   The AI may produce a different output (LLMs are stochastic), but the
   replay will show what *could* have happened.

2. **Temperature control:** Trading decisions use `temperature=0.3` (lower
   than creative tasks) for more consistent outputs.

3. **Decision fingerprints:** Every decision is hashed with its inputs.
   If the same inputs appear again, the AI sees its previous decision and
   must explicitly justify any change.

4. **Behavioral boundaries:** The system prompt explicitly constrains:
   - Never trade more than 3 assets simultaneously
   - Never use more than 50% of available capital in one cycle
   - Always prefer fewer, higher-conviction triggers over many low-conviction ones

---

## Budget Enforcement

```rust
struct AgentBudget {
    steps_remaining: u8,     // Max 20
    sub_agent_budget: u8,    // Max 5 delegations
    search_budget: u8,       // Max 3 web searches
    time_remaining_ms: u64,  // Max 120_000 (2 minutes)
}

impl AgentBudget {
    fn can_act(&self) -> bool {
        self.steps_remaining > 0 && self.time_remaining_ms > 5000
    }

    fn must_finish(&self) -> bool {
        self.steps_remaining <= 3 || self.time_remaining_ms < 30000
    }

    fn spend_step(&mut self) {
        self.steps_remaining = self.steps_remaining.saturating_sub(1);
    }
}
```

When `must_finish()` is true, the AI skips optional steps (debate, detailed
analysis) and produces a fast decision based on whatever it has gathered.

---

## Tiered Execution & API Budget

See [vulnerability-response.md](vulnerability-response.md#v1-api-rate-limiting--token-exhaustion-high) for full analysis.

### Tier Selection

| Tier | Trigger Condition | API Calls | Debate |
|---|---|---|---|
| 1: FAST TRACK | Trade < 5% of portfolio, confidence > 80% | 2 | Skipped |
| 2: STANDARD | Trade 5-10% or moderate confidence | 4 | Combined if budget |
| 3: DEEP | Trade > 8% or new asset or volatile market | 7-9 | Full debate |

### API Budget Per Cycle

- Groq: max 5 calls · Gemini: max 3 · OpenRouter: max 5
- Total tokens per cycle: max 50k
- Rate limit prediction: track 429 errors, deprioritize near-limit providers

### Sub-Agent Priority Queue

Not all 7 sub-agents run every cycle. AI selects 2-3 most relevant:
- **Always:** Macro Analyst (cheap, fast, high value)
- **Conditional:** Technical (near S/R), News (>5% move), On-Chain (volume anomaly)
- **Rare:** Search Agent (only when AI explicitly requests unknown info)
- **On-demand:** Risk Auditor (>8% position), Self-Critique (post-hypothesis)

Parallel sub-agents are **batched** into a single API call when 3+ are needed.

---

## Atomic Commit (Crash Safety)

See [vulnerability-response.md](vulnerability-response.md#v7-mid-cycle-crash-desync-high) for full analysis.

The AI never modifies live state directly. It works on a **draft**:

1. Clone portfolio state → draft_state
2. AI works on draft for up to 20 steps
3. All actions accumulated in draft
4. **ATOMIC COMMIT:** journal written first (write-ahead log) → actions applied → SQLite persisted
5. ANY step fails → ROLLBACK entire cycle

On restart, incomplete cycles are detected and replayed from the journal.
Crash at any point cannot leave the portfolio in an inconsistent state.
