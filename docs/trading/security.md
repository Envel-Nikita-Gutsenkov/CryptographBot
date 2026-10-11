# Security Measures — Trading System

Security is layered: the paper trading sandbox inherits all existing Cryptograph
security measures and adds trading-specific protections.

---

## Existing Security (Inherited)

From the main Cryptograph system:

| Measure | Description |
|---|---|
| Admin-only access | `ALLOWED_ADMIN_IDS` middleware on all handlers |
| SQLite-first storage | Single `crypto.db` (WAL); hot portfolio state in RAM, persisted as versioned JSON |
| Ephemeral session key | Random 32-byte key per process, zeroized on drop (no `MASTER_KEY` env) |
| No inbound ports | Long-polling only, zero firewall config |
| Read-only Docker rootfs | Container filesystem is immutable |
| Rate limiting | Market-data quota protection via TTL caches + consensus |
| Plain-JSON backups | `/backup` exports portable JSON — **not encrypted**, treat as sensitive |
| Keys in `.env` only | Real API keys never committed; `.env` is gitignored |

> [!NOTE]
> Earlier versions encrypted serialized data with AES-256-GCM under a
> `MASTER_KEY`. That design was replaced by SQLite-first persistence with
> plain-JSON backups; there is no `MASTER_KEY` anymore.

---

## Trading-Specific Security Measures

### 1. AI Cannot Execute Trades Directly

The AI has **zero access** to the Execution Engine or trade execution functions.
It can only call `set_trigger()` and `cancel_trigger()` — both go through
validation before affecting the virtual portfolio.

```
AI → set_trigger(params) → Validate params → Create ActiveTrigger → (later) Engine evaluates
                                                                → AI CANNOT force execution
```

### 2. Risk Manager Is Immutable to AI

The `risk.rs` module is a pure function with no API exposed to the AI.
The AI cannot:
- Change risk parameters
- Bypass risk checks
- Disable circuit breakers
- Unlock locked capital

Only the human admin can modify risk parameters via `/trade risk set`.

### 3. Emergency Stop

A single command (`/trade stop`) cancels ALL active triggers and pauses the
Execution Engine. This is the nuclear option — no AI action can override it.

```rust
async fn emergency_stop(store: &Arc<Store>, sqlite: &Arc<SqliteStore>) {
    let mut state = store.write();

    // Cancel all active triggers
    for trigger in &mut state.active_triggers {
        trigger.status = TriggerStatus::Cancelled;
    }

    // Set emergency flag
    state.trading_halted = true;

    // Persist
    sqlite.cancel_all_triggers().await;
    sqlite.set_trading_halted(true).await;

    // Notify user
    tracing::warn!("EMERGENCY STOP: All trading halted by user command");
}
```

### 4. Dead Man's Switch

If the AI brain process is offline for too long, all triggers auto-expire:

```rust
// On every engine tick:
for trigger in &active_triggers {
    let age = Utc::now() - trigger.created_at;
    if age > Duration::hours(config.trigger_max_age_hours) {
        expire_trigger(trigger);
        notify_user("Trigger #{} expired: no AI check-in for {}h", trigger.id, age.hours());
    }
}
```

Additionally, if the AI hasn't checked in for 2x the normal cycle interval,
the user is alerted via Telegram.

### 5. Audit Trail

Every action is logged immutably:

| Action | Logged To |
|---|---|
| AI cycle runs | `trading_journal` table |
| Trigger created | `active_triggers` table + journal |
| Trigger cancelled | `active_triggers` (status=Cancelled) + journal |
| Trade executed | `virtual_trades` table |
| Trade vetoed | `risk_vetoes` table + trigger status |
| Risk param changed | `audit_log` table |
| Emergency stop | `audit_log` table |
| Strategy changed | Memo entry in portfolio |

Full `audit_log` schema:

```sql
CREATE TABLE IF NOT EXISTS audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL,
    actor TEXT NOT NULL,       -- 'user', 'ai_brain', 'execution_engine', 'risk_manager'
    action TEXT NOT NULL,      -- 'trigger_created', 'trade_executed', 'emergency_stop', etc.
    details TEXT NOT NULL,     -- JSON with full context
    user_id INTEGER            -- Telegram user ID if action by user
);
```

### 6. Paper Trading Isolation

The virtual portfolio is completely isolated from any real wallet data:

```rust
// Virtual portfolio is stored separately
pub struct StoreData {
    // ... existing real portfolio fields ...
    pub portfolio: Portfolio,           // REAL portfolio (manual tracking)

    // Paper trading fields (completely separate)
    pub virtual_portfolio: Option<VirtualPortfolio>,  // PAPER trading
    pub active_triggers: Vec<ActiveTrigger>,
    pub trading_halted: bool,
}
```

The Execution Engine only reads `virtual_portfolio` — it CANNOT access real
wallet data or real portfolio data. Even if the AI somehow generated real
transaction data, the engine would reject it because it only works with
virtual assets.

### 7. Rate Limiting on AI Actions

The AI cannot flood the system with triggers:

```rust
const MAX_TRIGGERS_PER_CYCLE: usize = 10;
const MAX_ACTIVE_TRIGGERS_TOTAL: usize = 20;
const MIN_CYCLE_INTERVAL_MINUTES: u64 = 30; // AI cannot cycle faster than this
```

### 8. Input Validation

All AI tool parameters are strictly validated:

```rust
fn validate_set_trigger(params: &SetTriggerParams, portfolio: &VirtualPortfolio) -> Result<(), ValidationError> {
    // Symbol must be in allowed list
    if !risk_params.allowed_assets.contains(&params.symbol) {
        return Err(ValidationError::SymbolNotAllowed(params.symbol.clone()));
    }

    // Amount must be positive
    if params.amount_usd <= 0.0 {
        return Err(ValidationError::InvalidAmount);
    }

    // Amount must be ≤ available balance (for buys)
    if params.action == TriggerAction::Buy && params.amount_usd > portfolio.available_usd {
        return Err(ValidationError::InsufficientBalance);
    }

    // Target price must be reasonable (0.01x – 100x current price)
    // Prevents absurd triggers like "Buy BTC at $1" or "Sell BTC at $1,000,000"
    let current_price = get_current_price(&params.symbol);
    if params.target_price < current_price * 0.01 || params.target_price > current_price * 100.0 {
        return Err(ValidationError::UnreasonablePrice);
    }

    // Trigger count limits
    if active_triggers_count >= MAX_ACTIVE_TRIGGERS_TOTAL {
        return Err(ValidationError::TooManyTriggers);
    }

    Ok(())
}
```

### 9. Prompt Injection Protection

The AI prompt is constructed server-side. User inputs (strategy directives,
memos) are sanitized:

```rust
fn sanitize_user_input(input: &str) -> String {
    // Remove any attempts to inject system instructions
    let cleaned = input
        .replace("<system>", "")
        .replace("</system>", "")
        .replace("<|im_start|>", "")
        .replace("<|im_end|>", "")
        .replace("[/INST]", "")
        .replace("[INST]", "")
        .trim()
        .to_string();

    // Truncate to reasonable length
    if cleaned.len() > 2000 {
        cleaned[..2000].to_string()
    } else {
        cleaned
    }
}
```

### 10. No Real Money Gateway (v1)

In the paper trading phase, there is **no code path** that connects to real
exchange APIs for trading. The only external calls are:
- Price data fetching (CMC, CoinGecko, etc.) — read-only
- AI model API calls (Gemini, Groq, etc.) — for analysis

No swap execution, no wallet signing, no transaction broadcasting.

When Phase 9 (hot wallet integration) is implemented, it will require:
- Separate feature flag (`--features live-trading`)
- Additional API keys (exchange API with trading permissions)
- Multi-signature or approval flow for real trades
- Separate audit and monitoring

---

## Security Checklist

- [ ] AI cannot execute trades directly (only set triggers)
- [ ] Risk Manager is immutable to AI
- [ ] Emergency stop cancels all triggers immediately
- [ ] Dead man's switch expires triggers after AI offline period
- [ ] Full audit trail for every action
- [ ] Paper trading completely isolated from real wallet data
- [ ] Rate limiting on AI trigger creation
- [ ] Input validation on all AI tool parameters
- [ ] User input sanitized before injection into AI prompts
- [ ] No real money gateway in v1
- [ ] SQLite-first persistence with plain-JSON backups (inherited)
- [ ] Admin-only access (inherited)
- [ ] Keys live only in `.env`, never committed (inherited)

---

## Threat Model

| Threat | Mitigation |
|---|---|
| AI hallucinates a trade | MoE Critic validates against ground-truth prices; risk manager vetoes unrealistic trades |
| AI tries to drain the portfolio | Max position size (10%), max concentration (40%), profit lock-in |
| AI panic-sells in a crash | Max drawdown lock (no selling below 50% of entry) |
| AI spams triggers | Max 10 triggers per cycle, max 20 total active |
| AI ignores user strategy | Strategy memos injected into every prompt; Critic checks consistency |
| Market data feed manipulation | Consensus median across 3+ providers; outlier detection (MAD) |
| Bot crashes / goes offline | Dead man's switch expires all triggers; SQLite persistence for recovery |
| User accidentally deletes everything | Versioned JSON backups sent on every mutation; restore flow |
| Malicious user input to AI | Input sanitization, length limits, no system prompt injection |
