# Stability, Fault Tolerance & Predictability

> **Role:** System Architect  
> **Principle:** The system must degrade gracefully, never corrupt data,
> recover automatically, and behave predictably within defined boundaries.

---

## Stability Guarantees

### 1. Data Integrity

| Guarantee | How |
|---|---|
| No lost trades | Every trade is written to SQLite BEFORE updating in-memory state |
| No double-execution | Trigger status is atomically updated with trade creation |
| No phantom holdings | Holdings are recalculable from trade history at any time |
| Crash-safe writes | SQLite WAL mode + synchronous=NORMAL |
| Backup consistency | Portfolio snapshot is taken atomically under write lock |

**Integrity verification command:**
```bash
cargo run --bin cryptograph -- trade-verify
```

This replays all trades from the journal, recalculates the portfolio state,
and compares it to the current in-memory state. Any discrepancy is flagged:

```
✅ Trade log: 18 trades, all valid
✅ Portfolio: recalculated $1,023.40 = stored $1,023.40
✅ Holdings: 2 positions, amounts match
✅ Triggers: 28 total, 3 active, statuses consistent
✅ Fees: $3.60 calculated = $3.60 stored
⚠️ WARNING: 1 expired trigger not marked (auto-fixed)
```

### 2. Crash Recovery

```
TIMELINE:
─────────────────────────────────────────────────────
NORMAL       CRASH!        RESTART        RECOVERED
│            │             │              │
├────────────┼─────────────┼──────────────┤
│  Running   │ Process     │ Bot starts   │ State loaded
│  normally  │ killed      │ from SQLite  │ from last
│            │ (power loss,│              │ consistent
│            │  OOM, etc.) │              │ snapshot
│            │             │              │
│  SQLite    │ Last write  │ SQLite WAL   │ Replay WAL
│  WAL mode  │ is safe     │ recovered    │ → consistent
│            │             │              │
│  In-memory │ LOST         │ Reloaded     │ Triggers
│  state     │ (acceptable) │ from SQLite  │ restored
└────────────┴─────────────┴──────────────┘
```

On restart:
1. SQLite WAL is replayed (automatic)
2. Virtual portfolio is reloaded from `virtual_trades` table
3. Active triggers are reloaded from `active_triggers` table
4. The last AI cycle timestamp is checked — if > 2x normal interval,
   the user is alerted: "Bot was offline for X hours. N triggers may have
   been missed. Review active triggers."
5. Execution Engine resumes from next tick

**What is lost on crash:** In-memory state since last SQLite write (at most
5 minutes of engine ticks). This is acceptable — the next tick will re-evaluate
all triggers against current prices.

### 3. Graceful Degradation

When external services fail, the system doesn't crash — it degrades:

| Service | Failure Mode | Degradation |
|---|---|---|
| CMC API | Timeout / 429 | Fall back to CoinGecko → CoinCap → cached prices |
| All price APIs | All down | Use last known prices with staleness warning. Triggers use stale prices but are marked "low confidence" |
| Gemini API | Timeout / 429 | Fall back to OpenRouter → Groq → Mistral → Nvidia |
| All AI APIs | All down | Skip AI cycle. Engine continues evaluating existing triggers. User alerted. |
| SQLite | Disk full | Stop writing journal. Keep in-memory state. Alert user. |
| Telegram API | Timeout | Queue messages. Retry with exponential backoff. |

### 4. State Consistency During Failures

The system uses a **write-ahead pattern**:

```
1. Validate action (risk check, balance check)
2. Write to SQLite (durable)
3. Update in-memory state (fast)
4. If step 3 fails → state is still consistent (SQLite is source of truth)
5. Next read reloads from SQLite
```

---

## Fault Tolerance Patterns

### Circuit Breakers (API Level)

```rust
struct ApiCircuitBreaker {
    failures: u32,
    last_failure: Instant,
    state: CircuitState, // Closed | HalfOpen | Open
    cooldown: Duration,
    max_failures: u32,
}

// Closed → normal operation
// After max_failures in window → Open (reject all calls)
// After cooldown → HalfOpen (allow 1 probe call)
// Probe succeeds → Closed
// Probe fails → Open again
```

Applied to: CMC API, CoinGecko API, each AI provider separately.

### Retry with Exponential Backoff

```
Attempt 1: immediate
Attempt 2: wait 1 second
Attempt 3: wait 2 seconds
Attempt 4: wait 4 seconds
Attempt 5: wait 8 seconds
→ Give up, use fallback
```

### Dead Letter Queue

If a trade execution fails after risk approval (e.g., SQLite write fails),
the trade is placed in a dead letter queue and retried on the next engine tick:

```
Tick N:   Trigger fires → Risk approved → SQLite write FAILS
Tick N+1: Dead letter queue: retry trade #FAILED-1
          → SQLite write succeeds → state updated
```

### Watchdog for Engine

A separate tokio task monitors the Execution Engine:

```rust
tokio::spawn(async {
    loop {
        sleep(Duration::from_secs(60)).await;
        let last_tick = engine_last_tick.load(Ordering::Relaxed);
        if elapsed_since(last_tick) > Duration::from_secs(600) {
            // Engine hasn't ticked in 10 minutes!
            tracing::error!("Execution Engine appears STALLED!");
            notify_user("⚠️ Trading engine stalled. Restarting...");
            // Trigger restart
        }
    }
});
```

---

## Predictability Boundaries

The system defines **explicit behavioral boundaries** — things the AI is
guaranteed NOT to do, regardless of what it "thinks":

### Hard Boundaries (Enforced by Code)

| Boundary | Enforcement |
|---|---|
| Never trade > 10% of balance per position | Risk Manager veto |
| Never sell below 50% of entry price | Risk Manager veto |
| Never hold > 40% in one asset | Risk Manager veto |
| Never exceed 20 active triggers | Trigger creation limit |
| Never run AI cycle more than once per 30 min | Rate limiter |
| Never execute a trigger older than 7 days | Dead man's switch |
| Never trade an unapproved asset | Asset whitelist |
| Never skip risk validation | All trades route through risk.rs |

### Soft Boundaries (Enforced by AI Prompt)

| Boundary | Prompt Instruction |
|---|---|
| Prefer 3-5 active triggers, not 15 | "Minimize trigger count. Quality over quantity." |
| Don't trade during weekends | "Avoid opening positions on Saturday-Sunday unless thesis is strong." |
| Don't chase pumps | "Never buy an asset up >15% in 24h. Wait for pullback." |
| Always have a thesis | "Every trigger must include a reasoning field. Empty = rejected." |
| Respect lessons learned | "You have N lessons from past cycles. Do not repeat documented mistakes." |

### Predictable Cycle Behavior

The AI cycle always follows the same structure:

```
1. Load context (memos, macro, portfolio)    → Always
2. Get market data (prices, news)             → Always
3. Analyze existing positions                 → Always
4. Delegate to sub-agents if needed           → Optional (budget permitting)
5. Form hypothesis                            → Always
6. Validate (MoE + Debate)                    → Always (unless budget exhausted)
7. Build proof                                → Always
8. Execute (set/cancel triggers)              → Always
9. Write memos                                → Always
```

The user knows exactly what to expect from each cycle. The only variable is
step 4 (sub-agents) — how deep the analysis goes depends on budget and
market conditions.

---

## Anomaly Detection

The system monitors itself for anomalies:

| Anomaly | Detection | Response |
|---|---|---|
| 5+ risk vetos in 10 minutes | `auto_revert_risk_params()` | Revert to last safe config |
| 3 consecutive losing trades | Alert user | "3 consecutive losses. Review strategy?" |
| AI hasn't checked in 24h+ | Dead man's switch | Alert user. Triggers still active but unmonitored. |
| Portfolio value diverges from recalculation | `trade-verify` | Flag for manual review |
| Price feed staleness > 10 min | Consensus check | Skip tick, use cached data with warning |
| SQLite database size > 100MB | Log rotation | Prune old journal entries |
| Memory usage > 500MB | GC old cycles from RAM | Keep only last 50 cycles in memory |

---

## Testing Stability

### Chaos Testing (Future)

```bash
cargo run --bin cryptograph -- trade-chaos \
    --kill-api-providers \
    --corrupt-sqlite-wal \
    --exhaust-disk-space \
    --flood-with-triggers \
    --random-restart
```

Each chaos scenario verifies the system degrades gracefully and recovers
without data loss or corruption.

### Soak Testing

```bash
cargo run --bin cryptograph -- trade-soak \
    --duration-days 30 \
    --accelerated-time 60x  # 1 real second = 1 simulated minute
```

Runs the full system for 30 simulated days in 12 real hours. Verifies:
- No memory leaks
- SQLite doesn't grow unbounded
- AI cycle times remain stable
- No cumulative drift in portfolio calculations

---

## Predictability Contract

The system makes these promises to the user:

1. **Your capital cannot be lost in one trade.** Max per-trade + max drawdown
   lock guarantee this.

2. **You will always know why a trade happened.** Every trade links to a
   Decision Proof with full evidence chain.

3. **You can stop everything with one command.** `/trade stop` cancels all
   triggers immediately. No delay, no override by AI.

4. **The system will tell you when something is wrong.** Anomalies are detected
   and reported proactively — not discovered later in logs.

5. **Your data survives crashes.** SQLite WAL + backup flow ensure no more
   than 5 minutes of state loss on catastrophic failure.

6. **The AI cannot go rogue.** Hard boundaries in code are stronger than any
   prompt instruction. The risk manager is immutable to AI.

7. **You can verify everything.** `trade-verify` recalculates all state from
   the immutable trade log. If numbers don't match, the system flags it.
