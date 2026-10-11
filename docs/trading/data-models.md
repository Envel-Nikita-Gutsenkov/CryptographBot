# Data Models — Paper Trading Sandbox

All models are serialized with `serde` and stored both in-memory (RAM Store) and persisted to SQLite.

---

## VirtualPortfolio

```rust
/// Represents the paper trading account state.
/// Lives in RAM (Store) and is persisted to SQLite on every mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualPortfolio {
    /// Initial capital deposited (never changes after init).
    pub initial_capital_usd: f64,

    /// Available USD balance for new trades (excludes locked capital).
    pub available_usd: f64,

    /// Capital that cannot be traded (profit lock-in: original deposit + accumulated lock).
    pub locked_capital_usd: f64,

    /// Current holdings: symbol → (amount_crypto, avg_cost_basis_per_unit_usd).
    /// Cost basis includes fees for accurate P&L.
    pub holdings: HashMap<String, VirtualHolding>,

    /// Monotonic trade ID counter.
    pub next_trade_id: u64,

    /// Timestamp of portfolio creation.
    pub created_at: DateTime<Utc>,

    /// Timestamp of last portfolio mutation.
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualHolding {
    /// Amount of cryptocurrency held.
    pub amount: f64,

    /// Average purchase price per unit, including fees.
    /// Calculated as: total_spent_usd / total_acquired_amount.
    pub avg_cost_basis: f64,

    /// Total USD spent acquiring this holding (including fees).
    pub total_cost_usd: f64,

    /// Timestamp of first purchase.
    pub first_purchase_at: DateTime<Utc>,

    /// Timestamp of last purchase.
    pub last_purchase_at: DateTime<Utc>,

    /// Number of separate purchase transactions.
    pub purchase_count: u32,
}
```

### Portfolio Methods

```rust
impl VirtualPortfolio {
    /// Total portfolio value at current prices: USD + sum(holdings × current_price).
    pub fn total_value_usd(&self, prices: &HashMap<String, f64>) -> f64;

    /// Unrealized P&L: total_value - (available_usd + locked_capital + total_cost_basis).
    pub fn unrealized_pnl_usd(&self, prices: &HashMap<String, f64>) -> f64;

    /// Realized P&L from closed trades in journal.
    pub fn realized_pnl_usd(&self, journal: &[VirtualTrade]) -> f64;

    /// Return on Investment: (total_value - initial_capital) / initial_capital × 100.
    pub fn roi_pct(&self, prices: &HashMap<String, f64>) -> f64;

    /// Can the portfolio open a new trade of amount_usd?
    pub fn can_trade(&self, amount_usd: f64) -> bool;
}
```

---

## ActiveTrigger

```rust
/// An algorithmic order placed by the AI, evaluated every engine tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveTrigger {
    /// Unique trigger ID (monotonic).
    pub id: u64,

    /// Target asset symbol (e.g., "BTC").
    pub symbol: String,

    /// The condition that must be met.
    pub condition: TriggerCondition,

    /// Price threshold that activates the trigger (USD).
    pub target_price: f64,

    /// What to do when triggered.
    pub action: TriggerAction,

    /// USD amount to trade.
    pub amount_usd: f64,

    /// AI cycle that created this trigger (for attribution).
    pub created_by_ai_cycle: String,

    /// When this trigger was created.
    pub created_at: DateTime<Utc>,

    /// Optional expiration: auto-cancel if not triggered by this time.
    pub expires_at: Option<DateTime<Utc>>,

    /// Current status.
    pub status: TriggerStatus,

    /// Number of engine ticks this trigger has survived.
    pub ticks_survived: u32,

    /// AI's reasoning for this trigger (saved for journal).
    pub ai_reasoning: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TriggerCondition {
    /// Execute when consensus price ≤ target_price.
    PriceBelow,

    /// Execute when consensus price ≥ target_price.
    PriceAbove,

    /// Activate trailing after price rises `activate_after_pct` above entry.
    /// Then sell if price drops `trail_step_pct` from peak.
    TrailingStop {
        activate_after_pct: f64,
        trail_step_pct: f64,
        /// Tracked internally by engine: highest price seen since activation.
        #[serde(skip)]
        peak_price: Option<f64>,
    },

    /// Staged profit-taking: list of (pct_gain, sell_fraction).
    /// e.g., [(10.0, 0.5), (20.0, 0.5)] = sell 50% at +10%, 50% at +20%.
    TakeProfit {
        stages: Vec<ProfitStage>,
    },

    /// Hard stop: sell all if price ≤ target.
    StopLoss,

    /// Auto-cancel after `duration_hours` without triggering.
    TimeDecay {
        duration_hours: f64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfitStage {
    pub gain_pct: f64,     // e.g., 10.0 for +10%
    pub sell_fraction: f64, // e.g., 0.5 for 50%
    pub executed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TriggerAction {
    /// Buy the asset with USD.
    Buy,
    /// Sell the asset for USD.
    Sell,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TriggerStatus {
    /// Waiting for condition to be met.
    Active,
    /// Condition met, trade executed.
    Executed,
    /// Cancelled by AI or user.
    Cancelled,
    /// Expired without triggering.
    Expired,
    /// Vetoed by risk manager (with reason).
    Vetoed { reason: String },
}
```

---

## VirtualTrade

```rust
/// A completed paper trade. Immutable after creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualTrade {
    /// Unique trade ID.
    pub id: u64,

    /// Asset symbol.
    pub symbol: String,

    /// Buy or Sell.
    pub side: TradeSide,

    /// Amount of cryptocurrency traded.
    pub amount_crypto: f64,

    /// Execution price per unit (USD).
    pub price_usd: f64,

    /// Total USD value before fees.
    pub gross_value_usd: f64,

    /// Fee deducted (USD).
    pub fee_usd: f64,

    /// Simulated slippage cost (USD).
    pub slippage_usd: f64,

    /// Net value after fees and slippage.
    pub net_value_usd: f64,

    /// Trigger that caused this trade (if any).
    pub trigger_id: Option<u64>,

    /// AI cycle that created the trigger.
    pub ai_cycle_id: Option<String>,

    /// Hash of AI reasoning for attribution.
    pub ai_reasoning_hash: String,

    /// Execution timestamp.
    pub executed_at: DateTime<Utc>,

    /// Consensus price sources used.
    pub price_sources: Vec<String>,

    /// Risk validation result.
    pub risk_check: RiskCheckResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TradeSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RiskCheckResult {
    Passed,
    OverrideByUser,
    Vetoed { rule: String, reason: String },
}
```

---

## TradingJournal

```rust
/// A complete record of one AI brain cycle.
/// Used for performance analysis and model training.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingJournalEntry {
    /// Unique cycle ID (timestamp + hash).
    pub cycle_id: String,

    /// When this AI cycle ran.
    pub timestamp: DateTime<Utc>,

    /// AI model(s) used in this cycle.
    pub models_used: Vec<String>,

    /// Market snapshot at time of decision (prices, changes, ATH discounts).
    pub market_snapshot: HashMap<String, MarketSnapshotEntry>,

    /// Portfolio state before AI decisions.
    pub portfolio_before: PortfolioSnapshot,

    /// AI's full reasoning (from MoE pipeline output).
    pub ai_reasoning: String,

    /// Triggers set during this cycle.
    pub triggers_set: Vec<TriggerSummary>,

    /// Triggers cancelled during this cycle.
    pub triggers_cancelled: Vec<u64>,

    /// User memos active at time of decision.
    pub active_memos: Vec<String>,

    /// Strategy directive active at time of decision.
    pub strategy_directive: String,

    /// Macro context at time of decision.
    pub macro_context: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketSnapshotEntry {
    pub symbol: String,
    pub price_usd: f64,
    pub change_1h_pct: Option<f64>,
    pub change_24h_pct: Option<f64>,
    pub change_7d_pct: Option<f64>,
    pub change_30d_pct: Option<f64>,
    pub ath_discount_pct: Option<f64>,
    pub volume_24h_usd: f64,
    pub market_cap_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioSnapshot {
    pub available_usd: f64,
    pub locked_capital_usd: f64,
    pub holdings: HashMap<String, f64>, // symbol → amount
    pub total_value_usd: f64,
    pub unrealized_pnl_usd: f64,
    pub realized_pnl_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerSummary {
    pub id: u64,
    pub symbol: String,
    pub condition: String,
    pub target_price: f64,
    pub action: String,
    pub amount_usd: f64,
}
```

---

## PerformanceAttribution

```rust
/// Links a trade outcome to the AI decision that caused it.
/// Enables "which AI model/strategy made the best decisions" analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceAttribution {
    pub trade_id: u64,
    pub ai_cycle_id: String,
    pub ai_model_generators: Vec<String>,
    pub ai_model_critic: String,
    pub strategy_label: String,
    pub symbol: String,
    pub side: TradeSide,
    pub amount_usd: f64,
    pub entry_price: f64,
    pub exit_price: Option<f64>,
    pub pnl_realized_usd: Option<f64>,
    pub pnl_pct: Option<f64>,
    pub fees_paid_usd: f64,
    pub holding_duration_hours: Option<f64>,
    pub was_vetoed: bool,
    pub veto_reason: Option<String>,
}
```

---

## SQLite Schema

```sql
CREATE TABLE IF NOT EXISTS virtual_trades (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    symbol TEXT NOT NULL,
    side TEXT NOT NULL CHECK(side IN ('Buy', 'Sell')),
    amount_crypto REAL NOT NULL,
    price_usd REAL NOT NULL,
    gross_value_usd REAL NOT NULL,
    fee_usd REAL NOT NULL DEFAULT 0.0,
    slippage_usd REAL NOT NULL DEFAULT 0.0,
    net_value_usd REAL NOT NULL,
    trigger_id INTEGER,
    ai_cycle_id TEXT,
    ai_reasoning_hash TEXT NOT NULL DEFAULT '',
    executed_at TEXT NOT NULL,
    price_sources TEXT NOT NULL DEFAULT '[]',
    risk_check_result TEXT NOT NULL DEFAULT 'Passed'
);

CREATE TABLE IF NOT EXISTS active_triggers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    symbol TEXT NOT NULL,
    condition_type TEXT NOT NULL,
    condition_data TEXT NOT NULL,  -- JSON
    action TEXT NOT NULL CHECK(action IN ('Buy', 'Sell')),
    amount_usd REAL NOT NULL,
    target_price REAL NOT NULL,
    created_by_ai_cycle TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT,
    status TEXT NOT NULL DEFAULT 'Active',
    ticks_survived INTEGER NOT NULL DEFAULT 0,
    ai_reasoning TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS trading_journal (
    cycle_id TEXT PRIMARY KEY,
    timestamp TEXT NOT NULL,
    models_used TEXT NOT NULL DEFAULT '[]',
    market_snapshot TEXT NOT NULL,   -- JSON
    portfolio_before TEXT NOT NULL,  -- JSON
    ai_reasoning TEXT NOT NULL,
    triggers_set TEXT NOT NULL,      -- JSON
    triggers_cancelled TEXT NOT NULL,-- JSON
    active_memos TEXT NOT NULL,      -- JSON
    strategy_directive TEXT NOT NULL DEFAULT '',
    macro_context TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS performance_log (
    trade_id INTEGER PRIMARY KEY,
    ai_cycle_id TEXT NOT NULL,
    ai_model_generators TEXT NOT NULL DEFAULT '[]',
    ai_model_critic TEXT NOT NULL DEFAULT '',
    strategy_label TEXT NOT NULL DEFAULT '',
    symbol TEXT NOT NULL,
    side TEXT NOT NULL,
    amount_usd REAL NOT NULL,
    entry_price REAL NOT NULL,
    exit_price REAL,
    pnl_realized_usd REAL,
    pnl_pct REAL,
    fees_paid_usd REAL NOT NULL DEFAULT 0.0,
    holding_duration_hours REAL,
    was_vetoed INTEGER NOT NULL DEFAULT 0,
    veto_reason TEXT
);

CREATE INDEX IF NOT EXISTS idx_trades_symbol ON virtual_trades(symbol);
CREATE INDEX IF NOT EXISTS idx_trades_executed ON virtual_trades(executed_at);
CREATE INDEX IF NOT EXISTS idx_triggers_status ON active_triggers(status);
CREATE INDEX IF NOT EXISTS idx_journal_timestamp ON trading_journal(timestamp);
CREATE INDEX IF NOT EXISTS idx_perf_symbol ON performance_log(symbol);
```

---

## In-Memory Store Extensions

The existing `StoreData` struct gains:

```rust
pub struct StoreData {
    // ... existing fields ...

    /// Virtual portfolio for paper trading.
    pub virtual_portfolio: Option<VirtualPortfolio>,

    /// Active algorithmic triggers set by AI.
    pub active_triggers: Vec<ActiveTrigger>,

    /// Trading journal entries (capped at last 1000, older pruned).
    pub trading_journal: Vec<TradingJournalEntry>,

    /// Persistent lessons learned by the AI.
    pub trading_lessons: Vec<Memo>,

    /// Current AI strategy directive.
    pub trading_strategy: Option<String>,

    /// Risk parameter overrides (user-configured).
    pub risk_params: RiskParameters,
}
```

---

## TradingMode (User-Controlled)

```rust
/// The bot's operating mode. User-controlled via /trade mode.
/// Injected into every AI prompt AND enforced by the Risk Manager.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TradingMode {
    /// Buy-only: bot accumulates assets at discounts, never sells.
    /// Exception: emergency thesis-invalidation sells (if user allows).
    Accumulate,

    /// Two-sided: full swing trading — buy on dips, sell on profits.
    Swing,
}

impl Default for TradingMode {
    fn default() -> Self {
        TradingMode::Swing // conservative default: two-sided is the full feature set
    }
}
```

### Stored in RiskParameters / StoreData

```rust
pub struct StoreData {
    // ...
    /// Current trading mode (user-controlled).
    pub trading_mode: TradingMode,

    /// Accumulate-mode emergency sell permission.
    pub allow_emergency_sells: bool, // default: true

    /// Accumulate-mode tiered deployment thresholds:
    /// (discount_pct, max_capital_fraction) pairs.
    /// Default: [(-10.0, 0.30), (-15.0, 0.30), (-25.0, 0.40)]
    pub accumulate_tiers: Vec<(f64, f64)>,
}
```
