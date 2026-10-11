# MoE Pipeline — Double-Checking for Trading

The AI Brain uses the existing Mixture of Experts (MoE) pipeline from
`crypto/ai/pipeline.rs` but with trading-specific prompts and validation.

---

## Pipeline Overview

```
                    ┌─────────────────┐
                    │  Market Context  │
                    │  + Portfolio     │
                    │  + Memos         │
                    │  + Macro         │
                    └────────┬────────┘
                             │
              ┌──────────────┴──────────────┐
              │                             │
     ┌────────▼────────┐          ┌────────▼────────┐
     │  Generator-A    │          │  Generator-B    │
     │  (Fast model)   │          │  (Fast model)   │
     │  e.g. Qwen-2.5  │          │  e.g. Llama-3   │
     └────────┬────────┘          └────────┬────────┘
              │                             │
              │    Draft-A (triggers)       │    Draft-B (triggers)
              └──────────────┬──────────────┘
                             │
              ┌──────────────▼──────────────┐
              │      CRITIC (Heavy model)   │
              │      e.g. Gemini 2.5 Pro    │
              │                             │
              │  Cross-references:          │
              │  ✓ Prices match snapshot?   │
              │  ✓ Within risk limits?      │
              │  ✓ Consistent with memos?   │
              │  ✓ No hallucinated data?    │
              │  ✓ Strategy logic sound?    │
              └──────────────┬──────────────┘
                             │
                    ┌────────▼────────┐
                    │  Final Output   │
                    │  (validated     │
                    │   triggers +    │
                    │   reasoning)    │
                    └─────────────────┘
```

---

## Generator Prompt Template

```text
You are an autonomous crypto trading AI strategist operating a paper trading
portfolio. Your goal: maximize risk-adjusted returns over a 1-4 week horizon.
You trade on "short long-term" swings — buying dips, selling rips.

## CRITICAL RULES (you cannot violate these):
1. Never hallucinate prices. Always use the Market Snapshot provided below.
2. Never exceed 10% of available balance per trade.
3. Never sell an asset below 50% of your entry price.
4. Always set an expiration on your triggers (max 7 days).
5. Always explain your reasoning in detail.
6. Respect user strategy memos — they override your own judgment.

## Current State

### Portfolio
{portfolio_state}

### Market Snapshot
{market_snapshot}

### Active Triggers
{active_triggers}

### Recent Trade History
{trade_history_summary}

### Your Past Memos
{memos}

### User Strategy Directive
{strategy_directive}

### Macro Context
{macro_context}

## Your Task
Analyze the current state and decide:
1. Should any existing triggers be cancelled? (market changed, strategy shift)
2. Should any new triggers be set? (buy dips, set take-profits, set stop-losses)
3. What is your reasoning? (save as a memo for your future self)

Output a JSON array of tool calls using the available tools:
- cancel_trigger(trigger_id, reason)
- set_trigger(symbol, condition, target_price, action, amount_usd, reasoning, ...)
- write_memo(text)

DO NOT output anything except valid JSON.
```

---

## Critic Prompt Template

```text
You are a strict trading risk auditor (CRITIC). Two independent AI Generators
have proposed trading actions for a crypto portfolio. Your job is to validate
their proposals and produce the FINAL, CORRECT set of actions.

## CRITICAL AUDIT RULES:
1. **Price Fact-Check:** Verify every price mentioned against the Ground-Truth
   Market Snapshot. If a Generator used a price not in the snapshot, REJECT
   that action and explain why.
2. **Risk Limit Check:** Verify every proposed trade against the Risk Limits.
   If an action violates any limit, REJECT it.
3. **Memo Consistency:** Check if any action contradicts User Strategy Memos.
   If so, REJECT it.
4. **Logic Check:** Does the reasoning make sense? Is the trade direction
   consistent with the stated strategy? Flag contradictions.
5. **No New Inventions:** Do NOT add triggers that neither Generator proposed.
   Only synthesize from their drafts.

## Ground-Truth Market Snapshot
{market_snapshot}

## Risk Limits
- Max position: 10% of available balance
- No selling below 50% of entry price
- Wash sale window: 48 hours
- Max concentration: 40% per asset

## Active User Strategy Memos
{memos}

## Generator-A Proposal
{generator_a_output}

## Generator-B Proposal
{generator_b_output}

## Your Output
Produce the FINAL validated JSON array of tool calls. For any rejected action,
include it with status "rejected" and a reason. For approved actions, output
them as valid tool calls.

Format:
```json
{
    "audit_summary": "Brief explanation of what you accepted, rejected, and why",
    "tool_calls": [
        // Only approved, corrected tool calls
    ],
    "rejected_actions": [
        {
            "generator": "A or B",
            "original_action": "...",
            "rejection_reason": "..."
        }
    ]
}
```
```

---

## Trade-Specific Critic Validations

The Critic performs these additional checks beyond the standard MoE pipeline:

### 1. Price Hallucination Detection
```text
FOR EACH trigger in drafts:
    IF trigger.target_price is NOT within 0.1% of snapshot.price_usd
       AND trigger.target_price is NOT a round number near technical levels:
        FLAG as potential hallucination
        → Compare against 7d high/low to see if it's a real level
        → If no match: REJECT
```

### 2. Strategy Contradiction Detection
```text
READ all active memos.
FOR EACH trigger in drafts:
    IF memo says "accumulate BTC on dips":
        AND trigger is SELL BTC at current levels:
            FLAG contradiction
            → REJECT unless Generator explicitly explains why strategy changed
```

### 3. Risk Pre-Check
```text
FOR EACH BUY trigger:
    IF amount_usd > portfolio.available_usd * 0.10:
        → REJECT (position size)
    IF holding_value_after_trade / portfolio_total > 0.40:
        → REJECT (concentration)

FOR EACH SELL trigger:
    IF trigger.target_price < entry_price * 0.50:
        → REJECT (drawdown lock)
```

### 4. Correlation Awareness
```text
IF portfolio already holds ETH AND trigger is BUY BTC:
    IF BTC-ETH 30d correlation > 0.85:
        → WARN (high correlation — portfolio not diversifying)
        → ACCEPT with warning flag (user can decide)
```

---

## Synthesis Logic

When Generators A and B produce conflicting proposals, the Critic must resolve:

| Scenario | Critic Action |
|---|---|
| Both propose same trigger | Accept (consensus = high confidence) |
| A proposes BUY, B proposes nothing | Accept if passes all checks |
| A proposes BUY at $60k, B proposes BUY at $58k | Take lower (more conservative) price |
| A proposes BUY, B proposes SELL same asset | REJECT both. Flag as "model disagreement". Write memo for human review. |
| A proposes 10%, B proposes 5% | Take lower amount (conservative) |
| One hallucinates price, one is correct | Accept the correct one, flag hallucination |

---

## Adversarial Debate Mode (Devil's Advocate)

After the Critic validates and synthesizes a final plan, an **Adversarial
Round** is triggered. A separate AI model (the "Challenger") is tasked with
**actively trying to disprove and destroy the plan**.

### Purpose

The Critic validates facts. The Challenger validates *logic* — it stress-tests
the strategy against worst-case scenarios, historical analogues, and hidden
assumptions.

### Debate Flow

```
                    ┌──────────────────────────┐
                    │  CRITIC OUTPUT           │
                    │  (validated triggers +    │
                    │   reasoning)              │
                    └────────────┬─────────────┘
                                 │
                    ┌────────────▼─────────────┐
                    │  CHALLENGER (Opponent)    │
                    │  "I will destroy this     │
                    │   plan. Here's why it     │
                    │   will fail..."           │
                    └────────────┬─────────────┘
                                 │
                    ┌────────────▼─────────────┐
                    │  DEFENDER (Original plan) │
                    │  "Here's why the          │
                    │   challenger is wrong..." │
                    └────────────┬─────────────┘
                                 │
                    ┌────────────▼─────────────┐
                    │  ARBITER (Heavy model)    │
                    │  Evaluates both sides.    │
                    │  Scores each argument.    │
                    │  Produces FINAL verdict.  │
                    └────────────┬─────────────┘
                                 │
                    ┌────────────▼─────────────┐
                    │  FINAL OUTPUT             │
                    │  (with debate score +     │
                    │   confidence level)       │
                    └──────────────────────────┘
```

### Challenger Prompt

```text
You are the DEVIL'S ADVOCATE. Your job is to ATTACK and DESTROY the following
trading plan. Find every weakness, every hidden assumption, every way this
could go wrong.

## The Plan (to be attacked)
{critic_output}

## Attack Vectors (you MUST address ALL of these):

1. **Counter-Evidence:** Is there historical data that contradicts this plan?
   "In the last 3 times {symbol} was at this level, it did the OPPOSITE..."

2. **Hidden Assumptions:** What is the plan assuming that might not be true?
   "This plan assumes support will hold, but volume is declining..."

3. **Worst-Case Scenario:** If everything goes wrong, what is the maximum loss?
   "If {symbol} drops to {worst_case}, this position loses ${amount}..."

4. **Correlation Trap:** Is the plan ignoring correlated risks?
   "Portfolio already has {other_symbol} which is 0.87 correlated..."

5. **Macro Contradiction:** Does the macro context contradict this plan?
   "Macro shows distribution phase, but this plan is accumulating..."

6. **Timing Attack:** Is the timing/expiration unrealistic?
   "This plan expects a 15% move in 7 days, but average 7d move is 8%..."

7. **Fee & Slippage Reality:** Will costs eat the expected profit?
   "Expected profit = $X, but round-trip costs = $Y (Z% of profit)..."

Output a structured attack with CONFIDENCE scores for each vector.
```

### Defender Prompt

```text
You are the DEFENDER. The Challenger has attacked your trading plan.
Defend it — but be HONEST. If the challenger has a valid point, concede it.

## Your Original Plan
{original_plan}

## Challenger's Attack
{challenger_output}

For each attack vector:
1. **REFUTE** — if the attack is invalid, explain why with evidence
2. **CONCEDE** — if the attack is valid, acknowledge it
3. **MITIGATE** — if the attack is partially valid, adjust the plan to address it

If you must concede on a critical point, PROPOSE A MODIFIED PLAN that
addresses the weakness.
```

### Arbiter Prompt

```text
You are the ARBITER. You have witnessed a debate between a trading strategist
(Defender) and a devil's advocate (Challenger).

## Original Plan
{original_plan}

## Challenger's Attack
{challenger_output}

## Defender's Response
{defender_output}

Evaluate each attack vector and produce:

```json
{
    "verdict": "APPROVED" | "APPROVED_WITH_MODIFICATIONS" | "REJECTED",
    "confidence": 0.0-1.0,
    "debate_score": {
        "challenger_points_won": N,
        "defender_points_won": N,
        "total_attack_vectors": 7
    },
    "attack_resolutions": [
        {
            "vector": "counter_evidence",
            "challenger_valid": true/false,
            "defender_response_adequate": true/false,
            "winner": "challenger" | "defender" | "draw",
            "impact_on_plan": "none" | "minor_adjustment" | "major_revision" | "fatal"
        }
    ],
    "modified_plan": null or {updated triggers},
    "risk_assessment": {
        "pre_debate_risk": "low/medium/high/critical",
        "post_debate_risk": "low/medium/high/critical",
        "risk_increased": true/false
    },
    "arbiter_reasoning": "Detailed explanation of the verdict..."
}
```

## Decision Rules:
- If ANY attack vector has "fatal" impact → REJECT the plan
- If challenger wins ≥4 of 7 vectors → APPROVED_WITH_MODIFICATIONS
- If defender wins ≥5 of 7 vectors → APPROVED with high confidence
- If risk_increased → require user approval before execution (for real trading)
```

### Debate Scoring Matrix

| Challenger Wins | Defender Wins | Verdict | Requires |
|---|---|---|---|
| 6–7 | 0–1 | **REJECTED** | Plan discarded, AI retries next cycle |
| 4–5 | 2–3 | **MODIFIED** | Plan adjusted per Arbiter, re-validated |
| 2–3 | 4–5 | **APPROVED** | Plan executed, debate logged for review |
| 0–1 | 6–7 | **STRONG_APPROVED** | Plan executed with high confidence marker |

### Debate Logging

Every debate is fully logged:

```json
{
    "cycle_id": "2026-08-05T14:00:00Z-abc123",
    "debate_id": "debate-abc123",
    "timestamp": "2026-08-05T14:05:00Z",
    "models": {
        "challenger": "claude-3.5-sonnet (openrouter)",
        "defender": "qwen-2.5-72b (groq)",
        "arbiter": "gemini-2.5-pro"
    },
    "verdict": "APPROVED_WITH_MODIFICATIONS",
    "confidence": 0.72,
    "debate_score": { "challenger": 3, "defender": 4 },
    "attack_resolutions": [
        {"vector": "counter_evidence", "winner": "defender", "impact": "none"},
        {"vector": "hidden_assumptions", "winner": "challenger", "impact": "minor_adjustment"},
        {"vector": "correlation_trap", "winner": "challenger", "impact": "minor_adjustment"},
        {"vector": "macro_contradiction", "winner": "defender", "impact": "none"},
        {"vector": "timing_attack", "winner": "challenger", "impact": "minor_adjustment"},
        {"vector": "fee_reality", "winner": "defender", "impact": "none"},
        {"vector": "worst_case", "winner": "defender", "impact": "none"}
    ],
    "modifications_made": [
        "Reduced ETH position size from 10% to 7% due to correlation concern",
        "Extended trigger expiration from 72h to 120h based on timing analysis"
    ],
    "risk_shift": { "pre": "medium", "post": "medium" }
}
```

## Decision Proofs (Evidence Chain)

Every AI trading decision must produce a **verifiable evidence chain** — a
structured proof that the decision was:
1. Based on real data (not hallucinated)
2. Consistent with the stated strategy
3. Within all risk limits
4. Survived adversarial challenge

### Proof Structure

```json
{
    "decision_id": "dec-2026-08-05-abc123",
    "timestamp": "2026-08-05T14:10:00Z",

    "evidence_chain": {
        "data_sources": {
            "prices_from": "consensus_median(cmc, coingecko, binance) at 14:00 UTC",
            "prices_hash": "sha256(market_snapshot)",
            "portfolio_from": "in_memory_store at 14:00 UTC",
            "macro_from": "macro_worker cache updated 12:00 UTC"
        },

        "reasoning_trace": [
            {
                "step": 1,
                "action": "read_market_snapshot(['BTC','ETH','SOL'])",
                "finding": "BTC=$65432, ETH=$2612, SOL=$142. All within expected ranges."
            },
            {
                "step": 2,
                "action": "review_trade_history(30)",
                "finding": "SOL trades +8.3% avg. ETH trades -3.1% avg. Pattern: ETH entries too early."
            },
            {
                "step": 3,
                "action": "read_memos()",
                "finding": "Lesson: 'Wait 24h after 5%+ drops before buying.' ETH dropped 6% today."
            },
            {
                "step": 4,
                "action": "DECISION: Do NOT buy ETH. Wait per lesson.",
                "basis": "Lesson from 2026-08-05. ETH drop 6% today = violates 24h wait rule."
            },
            {
                "step": 5,
                "action": "set_trigger(SOL, PriceBelow, $130, Buy, $50)",
                "basis": "SOL at $142. Support at $130 held 3 times in 30 days. 8.5% discount from current."
            }
        ],

        "risk_validation": {
            "max_position_pct": { "limit": 0.10, "actual": 0.05, "passed": true },
            "max_concentration": { "limit": 0.40, "actual": 0.22, "passed": true },
            "wash_sale": { "last_sold": null, "passed": true },
            "min_liquidity": { "required": 500.0, "actual_24h_vol": 2800000000.0, "passed": true },
            "max_slippage": { "limit": 0.02, "estimated": 0.00002, "passed": true }
        },

        "moe_validation": {
            "generators_agreed": true,
            "critic_approved": true,
            "hallucinations_detected": 0
        },

        "adversarial_debate": {
            "verdict": "APPROVED",
            "confidence": 0.85,
            "challenger_wins": 1,
            "defender_wins": 6,
            "fatal_attacks": 0
        },

        "strategy_consistency": {
            "active_directive": "Focus on accumulation during corrections...",
            "decision_aligned": true,
            "alignment_score": 0.92
        }
    },

    "verifiable_claims": [
        {
            "claim": "SOL support at $130 held 3 times in 30 days",
            "verification": "Check SOL 30d price history: lows at $128.5 (Jul 15), $131.2 (Jul 22), $129.8 (Jul 30)",
            "verified": true
        },
        {
            "claim": "ETH dropped 6% today",
            "verification": "ETH change_24h_pct from market snapshot = -6.1%",
            "verified": true
        }
    ]
}
```

### Proof Verification (User-Facing)

When the user clicks "📝 AI Reasoning" in the Trading Panel, they see not
just the decision but the **evidence chain with verification status**:

```
📝 AI DECISION — Aug 5, 2026 14:10 UTC

Decision Confidence: 🟢 HIGH (85%)
Debate Result: Defender won 6/7 rounds

━━━━━━━━━━━━━━━━━━━━━━━━━━

✅ SET TRIGGER: Buy SOL at $130.00 ($50)

Evidence:
  ✓ SOL at $142. Support $130 held Jul 15, 22, 30
  ✓ Within 5% position limit (5.0% vs 10% max)
  ✓ 24h volume $2.8B — liquidity OK (56000x trade)
  ✓ No wash sale (last SOL trade: none)
  ✓ Generators agreed + Critic approved
  ✓ Adversarial debate: 6/7 attacks defeated

━━━━━━━━━━━━━━━━━━━━━━━━━━

❌ REJECTED: Buy ETH at $2400

Evidence:
  ✗ ETH dropped 6.1% today
  ✗ Lesson "Wait 24h after 5%+ drops" applies
  ✗ Past 2 ETH entries on drop day lost -$30.50

━━━━━━━━━━━━━━━━━━━━━━━━━━

[📋 Full Evidence Chain]  [« Back]
```

## Performance Tracking

Every Critic decision is logged with attribution:

```json
{
    "cycle_id": "2026-08-05T14:00:00Z-abc123",
    "generator_a_model": "qwen-2.5-72b (groq)",
    "generator_b_model": "llama-3.1-70b (openrouter)",
    "critic_model": "gemini-2.5-pro",
    "actions_proposed": 5,
    "actions_accepted": 3,
    "actions_rejected": 2,
    "rejection_reasons": [
        "price_hallucination: Generator-B used BTC=$70000 but snapshot shows $65432",
        "risk_violation: SOL position would reach 45% concentration (max 40%)"
    ],
    "consensus_level": "high"  // both generators agreed on accepted actions
}
```

This data feeds the Performance Reviewer to identify which models hallucinate
most and which combinations produce the best results.

---

## Recalibrated Debate Scoring (Anti-Paralysis)

See [vulnerability-response.md](vulnerability-response.md#v3-adversarial-debate-paralysis-medium) for full analysis.

### Attack Classification (Weighted)

| Attack Type | Weight | Example |
|---|---|---|
| **FATAL** — Asset-specific, verifiable, immediate | 3 points | "SOL has $300M unlock in 2 days" |
| **SIGNIFICANT** — Asset-specific, probable | 2 points | "SOL RSI shows bearish divergence on daily" |
| **MODERATE** — Market-wide or uncertain | 1 point | "BTC dominance rising, alts may suffer" |
| **GENERIC** — Always true, not actionable | 0 points (REJECTED) | "Crypto is volatile" |

### Mitigation Discount

If Defender acknowledges and mitigates an attack, Arbiter discounts it:
- Full mitigation (stop-loss, reduced size): discount ×0.3
- Partial mitigation: discount ×0.6
- No mitigation: ×1.0

### Rejection Thresholds

| Challenger Score | Verdict |
|---|---|
| 0-3 | APPROVED |
| 4-6 | MODIFIED |
| 7-10 | REJECTED |
| 11+ | Only if MULTIPLE fatal flaws simultaneously |

### Rejection Rate Monitoring

If rejection rate > 50%: warn user ("AI rejecting >50% of plans. Review strictness?")
If rejection rate > 80%: auto-switch to lenient mode (only FATAL attacks count)
