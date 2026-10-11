# Risk Management — Hardcoded Safety Rules

The Risk Manager is a **pure function layer** that the AI cannot access, modify,
or override. Every trade proposal passes through these rules. A veto is final.

---

## Risk Rule Engine

```rust
/// Evaluate all risk rules against a proposed trade.
/// Returns Approved or Vetoed with the specific rule that blocked it.
pub fn evaluate_trade_risk(
    trigger: &ActiveTrigger,
    portfolio: &VirtualPortfolio,
    current_price: f64,
    prices: &HashMap<String, f64>,
    trade_history: &[VirtualTrade],
    risk_params: &RiskParameters,
    btc_price: Option<f64>,
    btc_price_1h_ago: Option<f64>,
) -> RiskResult {
    // Rules are evaluated in priority order.
    // First veto wins — subsequent rules are not checked.

    let rules: Vec<Box<dyn RiskRule>> = vec![
        Box::new(CircuitBreakerRule),
        Box::new(MaxPositionSizeRule),
        Box::new(MaxDrawdownLockRule),
        Box::new(MaxPortfolioConcentrationRule),
        Box::new(ProfitLockInRule),
        Box::new(MaxMonthlyDrawdownRule),
        Box::new(WashSalePreventionRule),
        Box::new(MinLiquidityRule),
        Box::new(MaxSlippageRule),
        Box::new(MaxDailyTradesRule),
        Box::new(MinHoldingPeriodRule),
        Box::new(AssetWhitelistRule),
    ];

    for rule in rules {
        if let Some(veto) = rule.check(trigger, portfolio, current_price, prices, trade_history, risk_params, btc_price, btc_price_1h_ago) {
            return RiskResult::Vetoed {
                rule: rule.name().to_string(),
                reason: veto,
            };
        }
    }

    RiskResult::Approved
}
```

---

## Rule Specifications

### 1. Circuit Breaker — `CircuitBreakerRule`

**Priority:** HIGHEST — checked first, halts ALL trading.

```
IF BTC price dropped >15% in the last 60 minutes:
    → VETO ALL TRADES (buy AND sell)
    → Reason: "Market circuit breaker: BTC dropped {pct}% in 1 hour"
    → Duration: 2 hours (configurable)
```

Rationale: During flash crashes, liquidity evaporates and prices are unreliable.
The bot must freeze until markets stabilize.

---

### 2. Max Position Size — `MaxPositionSizeRule`

```
IF trade.amount_usd > portfolio.available_usd * max_position_pct:
    → VETO
    → Reason: "Position size {amount_usd} exceeds {max_pct}% of available balance"
```

Default `max_position_pct`: **10%** (configurable per asset class)

Advanced: higher-volatility assets get a lower cap (e.g., SOL: 5%, BTC: 10%).

---

### 3. Max Drawdown Lock — `MaxDrawdownLockRule`

```
IF action == Sell AND current_price < entry_price * max_drawdown_pct:
    → VETO
    → Reason: "Cannot sell {symbol} at {current_price}: below {pct}% of entry {entry_price}"
```

Default `max_drawdown_pct`: **50%** (i.e., cannot sell below 50% of purchase price).

This prevents the AI from panic-selling at the bottom. If the asset truly needs
to be cut, the user must manually override.

---

### 4. Max Portfolio Concentration — `MaxPortfolioConcentrationRule`

```
IF action == Buy:
    let future_value = holding_value + trade.amount_usd
    let concentration = future_value / portfolio.total_value
    IF concentration > max_concentration_pct:
        → VETO
        → Reason: "{symbol} would reach {concentration}% of portfolio (max {max}%)"
```

Default `max_concentration_pct`: **40%** (no single asset > 40% of portfolio).

---

### 5. Profit Lock-In — `ProfitLockInRule`

```
IF portfolio.total_value >= initial_capital * profit_lock_multiplier:
    → Lock initial_capital_usd as untouchable
    → AI can only trade (total_value - initial_capital) = pure profit
```

Default `profit_lock_multiplier`: **2.0** (lock original capital at 2x).

Example: Start with $100 → reach $200 → lock $100 → AI can only risk the $100 profit.
If the portfolio later drops to $150, the locked $100 remains untouchable.

---

### 6. Max Monthly Drawdown — `MaxMonthlyDrawdownRule`

```
Calculate P&L for current calendar month from trade history.
IF monthly_pnl_pct < -max_monthly_drawdown_pct:
    → VETO ALL NEW BUYS
    → Allow only SELL (to exit positions)
    → Reason: "Monthly drawdown of {pct}% exceeds limit"
```

Default `max_monthly_drawdown_pct`: **-30%**.

---

### 7. Wash Sale Prevention — `WashSalePreventionRule`

```
IF action == Buy AND symbol was sold in the last wash_sale_window_hours:
    → VETO
    → Reason: "Cannot re-buy {symbol}: sold {hours}h ago (wash sale window)"
```

Default `wash_sale_window_hours`: **48**.

Prevents the AI from selling and immediately rebuying, which generates fees
without strategic benefit and can have tax implications.

---

### 8. Min Liquidity Check — `MinLiquidityRule`

```
IF 24h_volume_usd < trade.amount_usd * min_liquidity_multiplier:
    → VETO
    → Reason: "Insufficient liquidity: 24h volume ${volume} for trade ${amount}"
```

Default `min_liquidity_multiplier`: **10x** (24h volume must be at least 10x trade size).

---

### 9. Max Slippage Guard — `MaxSlippageRule`

```
estimated_slippage_pct = estimate_slippage(symbol, amount_usd, current_price)
IF estimated_slippage_pct > max_slippage_pct:
    → VETO
    → Reason: "Estimated slippage {pct}% exceeds max {max}%"
```

Default `max_slippage_pct`: **2%**.

Uses a simple model based on 24h volume: `slippage_pct = (amount_usd / volume_24h) * 100`.

---

### 10. Max Daily Trades — `MaxDailyTradesRule`

```
trades_today = count trades with executed_at within current UTC day
IF trades_today >= max_daily_trades:
    → VETO
    → Reason: "Daily trade limit of {max} reached"
```

Default `max_daily_trades`: **10**.

Prevents overtrading and excessive fee accumulation.

---

### 11. Min Holding Period — `MinHoldingPeriodRule`

```
IF action == Sell:
    holding_duration = now - holding.first_purchase_at
    IF holding_duration < min_holding_hours:
        → VETO
        → Reason: "Must hold {symbol} for at least {min}h (held for {duration}h)"
```

Default `min_holding_hours`: **6**.

Prevents ultra-short-term scalping. Aligns with the "short long-term" strategy.

---

### 12. Asset Whitelist — `AssetWhitelistRule`

```
IF symbol NOT IN risk_params.allowed_assets:
    → VETO
    → Reason: "{symbol} is not in the allowed trading list"
```

Default `allowed_assets`: `["BTC", "ETH", "SOL", "LINK"]` (user-configurable).

---

## User-Configurable Risk Parameters (with Safe Ranges)

Every parameter has a **safe range** — the user can adjust values freely WITHIN
this range. Values outside the range are rejected with an explanation.

### Risk Level System

Each parameter belongs to a **risk tier** that controls how easily it can be changed:

| Tier | Description | Change Requires |
|---|---|---|
| 🟢 **Conservative** | Safe defaults, narrow range | Instant via `/trade risk set` |
| 🟡 **Moderate** | Wider range, more risk | Confirmation prompt |
| 🔴 **Aggressive** | Maximum range, high risk | Explicit warning + 5-min cooldown |
| ⛔ **Forbidden** | Outside safe range | Rejected — value out of bounds |

### Control Levels (Escalation Model)

```
Level 0 — Silent      : Parameter change within conservative range → instant apply
Level 1 — Warning      : Parameter change into moderate range → confirmation required
Level 2 — Cooldown     : Parameter change into aggressive range → 5-min wait + warning
Level 3 — Blocked      : Parameter value outside absolute maximum → REJECTED
Level 4 — Emergency    : System detects anomaly → auto-revert to last safe config
```

### Integrity Verification

Every parameter change triggers:
1. **Bounds check** — is value within [min, max]?
2. **Consistency check** — do parameters contradict each other? (e.g., `min_holding_hours=1` + `wash_sale_window=720` is inconsistent)
3. **Hazard check** — would this parameter combination create a dangerous scenario? (e.g., `max_position_pct=50%` + `max_concentration_pct=90%` = possible 90% in one asset)
4. **History check** — has this parameter been changed too frequently? (max 3 changes per 24h per parameter)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskParameters {
    // === Position Sizing ===

    /// Max % of available balance per trade.
    /// Safe range: 0.01 – 0.25
    /// Conservative: 0.05, Moderate: 0.10, Aggressive: 0.20, Forbidden: >0.25
    pub max_position_pct: f64,            // default: 0.10

    /// Max % of portfolio in a single asset.
    /// Safe range: 0.10 – 0.60
    /// Conservative: 0.25, Moderate: 0.40, Aggressive: 0.55, Forbidden: >0.60
    pub max_concentration_pct: f64,       // default: 0.40

    // === Loss Protection ===

    /// Max % below entry price allowed for selling.
    /// Safe range: 0.20 – 0.80
    /// Conservative: 0.70, Moderate: 0.50, Aggressive: 0.30, Forbidden: <0.20
    /// NOTE: Lower = tighter stop, higher = more loss allowed before lock
    /// The lock prevents selling BELOW this % of entry. E.g., 0.50 = cannot sell below 50% of entry.
    pub max_drawdown_sell_pct: f64,       // default: 0.50

    /// BTC drop % in 1h that triggers circuit breaker.
    /// Safe range: 0.05 – 0.30
    /// Conservative: 0.10, Moderate: 0.15, Aggressive: 0.25, Forbidden: >0.30 or <0.05
    pub circuit_breaker_btc_drop_pct: f64, // default: 0.15

    /// Circuit breaker cooldown in hours.
    /// Safe range: 0.5 – 24.0
    /// Conservative: 4.0, Moderate: 2.0, Aggressive: 1.0, Forbidden: <0.5
    pub circuit_breaker_cooldown_hours: f64, // default: 2.0

    /// Max monthly P&L % before halting new buys.
    /// Safe range: -0.50 – -0.10
    /// Conservative: -0.15, Moderate: -0.30, Aggressive: -0.45, Forbidden: <-0.50 or >-0.10
    pub max_monthly_drawdown_pct: f64,    // default: -0.30

    // === Capital Protection ===

    /// Portfolio value multiplier that triggers capital lock-in.
    /// Safe range: 1.2 – 5.0
    /// Conservative: 1.5, Moderate: 2.0, Aggressive: 4.0, Forbidden: <1.2 or >5.0
    pub profit_lock_multiplier: f64,      // default: 2.0

    // === Trading Discipline ===

    /// Hours before same asset can be re-bought after selling.
    /// Safe range: 1.0 – 720.0 (30 days)
    /// Conservative: 72.0, Moderate: 48.0, Aggressive: 12.0, Forbidden: <1.0
    pub wash_sale_window_hours: f64,      // default: 48.0

    /// Minimum holding duration in hours.
    /// Safe range: 1.0 – 720.0 (30 days)
    /// Conservative: 24.0, Moderate: 6.0, Aggressive: 2.0, Forbidden: <1.0
    pub min_holding_hours: f64,           // default: 6.0

    /// Max trades per calendar day.
    /// Safe range: 1 – 50
    /// Conservative: 5, Moderate: 10, Aggressive: 30, Forbidden: >50
    pub max_daily_trades: u32,            // default: 10

    // === Market Quality ===

    /// Minimum 24h volume as multiple of trade size.
    /// Safe range: 2.0 – 50.0
    /// Conservative: 20.0, Moderate: 10.0, Aggressive: 5.0, Forbidden: <2.0
    pub min_liquidity_multiplier: f64,    // default: 10.0

    /// Max acceptable slippage %.
    /// Safe range: 0.005 – 0.05
    /// Conservative: 0.01, Moderate: 0.02, Aggressive: 0.04, Forbidden: >0.05
    pub max_slippage_pct: f64,            // default: 0.02

    // === Asset Control ===

    /// Allowed trading symbols (empty = all tracked).
    /// Safety: maximum 20 symbols. Must all be valid CoinMarketCap symbols.
    /// Verified against CMC on each change.
    pub allowed_assets: Vec<String>,      // default: ["BTC","ETH","SOL","LINK"]

    // === Risk Tier Override ===

    /// Maximum risk tier the user is willing to accept.
    /// "conservative" | "moderate" | "aggressive"
    /// Controls which parameter ranges are accessible.
    pub max_risk_tier: RiskTier,          // default: Moderate
}
```

### Parameter Validation Function

```rust
pub fn validate_risk_param_change(
    param_name: &str,
    new_value: f64,
    current_params: &RiskParameters,
    change_history: &[ParamChange],
) -> Result<RiskTier, ValidationError> {
    // 1. Get safe range for this parameter
    let range = get_safe_range(param_name);

    // 2. Bounds check
    if new_value < range.abs_min || new_value > range.abs_max {
        return Err(ValidationError::OutOfBounds {
            param: param_name.into(),
            value: new_value,
            min: range.abs_min,
            max: range.abs_max,
            reason: format!("Value {new_value} is outside absolute safe range [{}-{}]",
                range.abs_min, range.abs_max),
        });
    }

    // 3. Determine risk tier
    let tier = if new_value >= range.conservative_min && new_value <= range.conservative_max {
        RiskTier::Conservative
    } else if new_value >= range.moderate_min && new_value <= range.moderate_max {
        RiskTier::Moderate
    } else {
        RiskTier::Aggressive
    };

    // 4. Check if tier exceeds user's max allowed tier
    if tier as u8 > current_params.max_risk_tier as u8 {
        return Err(ValidationError::TierBlocked {
            requested: tier,
            allowed: current_params.max_risk_tier,
        });
    }

    // 5. History check — max 3 changes per 24h per parameter
    let changes_today = change_history.iter()
        .filter(|c| c.param == param_name && c.timestamp > Utc::now() - Duration::hours(24))
        .count();
    if changes_today >= 3 {
        return Err(ValidationError::TooFrequent {
            param: param_name.into(),
            changes_today,
        });
    }

    // 6. Consistency check — does this create contradictions?
    let simulated = current_params.with_change(param_name, new_value);
    if let Some(contradiction) = detect_contradictions(&simulated) {
        return Err(ValidationError::Inconsistent {
            contradiction,
        });
    }

    // 7. Hazard check — dangerous combinations?
    if let Some(hazard) = detect_hazards(&simulated) {
        return Err(ValidationError::Hazardous {
            hazard,
        });
    }

    Ok(tier)
}
```

---

## Parameter Safe Range Reference

| Parameter | Conservative | Moderate | Aggressive | Abs Min | Abs Max | Unit |
|---|---|---|---|---|---|---|
| `max_position_pct` | 0.05 | 0.10 | 0.20 | 0.01 | 0.25 | fraction |
| `max_concentration_pct` | 0.25 | 0.40 | 0.55 | 0.10 | 0.60 | fraction |
| `max_drawdown_sell_pct` | 0.70 | 0.50 | 0.30 | 0.20 | 0.80 | fraction |
| `circuit_breaker_btc_drop_pct` | 0.10 | 0.15 | 0.25 | 0.05 | 0.30 | fraction |
| `circuit_breaker_cooldown_hours` | 4.0 | 2.0 | 1.0 | 0.5 | 24.0 | hours |
| `max_monthly_drawdown_pct` | -0.15 | -0.30 | -0.45 | -0.50 | -0.10 | fraction |
| `profit_lock_multiplier` | 1.5 | 2.0 | 4.0 | 1.2 | 5.0 | multiplier |
| `wash_sale_window_hours` | 72.0 | 48.0 | 12.0 | 1.0 | 720.0 | hours |
| `min_holding_hours` | 24.0 | 6.0 | 2.0 | 1.0 | 720.0 | hours |
| `max_daily_trades` | 5 | 10 | 30 | 1 | 50 | count |
| `min_liquidity_multiplier` | 20.0 | 10.0 | 5.0 | 2.0 | 50.0 | multiplier |
| `max_slippage_pct` | 0.01 | 0.02 | 0.04 | 0.005 | 0.05 | fraction |

## Consistency & Hazard Detection

### Contradiction Detection

```rust
fn detect_contradictions(params: &RiskParameters) -> Option<String> {
    // Wash sale window should be ≥ min holding period (otherwise you can't sell then rebuy)
    if params.wash_sale_window_hours < params.min_holding_hours {
        return Some(format!(
            "CONTRADICTION: wash_sale_window ({wh}) < min_holding ({mh}) hours. \
            You must hold at least {mh}h, but wash sale allows re-buy after {wh}h. \
            Suggested: set wash_sale_window >= min_holding_hours.",
            wh = params.wash_sale_window_hours,
            mh = params.min_holding_hours
        ));
    }

    // Profit lock should be > 1.0 (otherwise capital is locked immediately)
    if params.profit_lock_multiplier <= 1.0 {
        return Some("CONTRADICTION: profit_lock_multiplier must be > 1.0".into());
    }

    // Max daily trades * max_position_pct should not allow >200% turnover
    let max_daily_turnover = params.max_daily_trades as f64 * params.max_position_pct;
    if max_daily_turnover > 2.0 {
        return Some(format!(
            "WARNING: max daily turnover = {:.0}% of portfolio possible. Consider tightening.",
            max_daily_turnover * 100.0
        ));
    }

    None
}
```

### Hazard Detection

```rust
fn detect_hazards(params: &RiskParameters) -> Option<String> {
    // HAZARD: max_position_pct * 2 > max_concentration_pct — two trades could over-concentrate
    if params.max_position_pct * 2.0 > params.max_concentration_pct {
        return Some(format!(
            "HAZARD: Two max-size trades ({:.0}% each) would reach {:.0}%, \
            exceeding concentration limit of {:.0}%.",
            params.max_position_pct * 100.0,
            params.max_position_pct * 200.0,
            params.max_concentration_pct * 100.0
        ));
    }

    // HAZARD: Very tight drawdown lock + wide position size = recipe for trapped capital
    if params.max_drawdown_sell_pct > 0.65 && params.max_position_pct > 0.15 {
        return Some(format!(
            "HAZARD: High drawdown lock ({:.0}%) + large position size ({:.0}%). \
            Capital could be trapped in losing positions.",
            params.max_drawdown_sell_pct * 100.0,
            params.max_position_pct * 100.0
        ));
    }

    // HAZARD: Circuit breaker too insensitive + aggressive position sizing
    if params.circuit_breaker_btc_drop_pct > 0.25 && params.max_position_pct > 0.20 {
        return Some(format!(
            "HAZARD: Circuit breaker triggers at {:.0}% BTC drop, but positions are {:.0}%. \
            Catastrophic loss possible before breaker activates.",
            params.circuit_breaker_btc_drop_pct * 100.0,
            params.max_position_pct * 100.0
        ));
    }

    None
}
```

### Auto-Revert (Level 4 — Emergency)

If the system detects an anomaly after a parameter change (e.g., 3 consecutive
vetoes in 10 minutes, or a trade that bypassed a rule due to a race condition),
it auto-reverts to the last known safe configuration:

```rust
pub fn auto_revert_if_anomaly(
    current: &RiskParameters,
    last_safe: &RiskParameters,
    recent_events: &[RiskEvent],
) -> Option<RiskParameters> {
    let vetoes_last_10min = recent_events.iter()
        .filter(|e| e.timestamp > Utc::now() - Duration::minutes(10))
        .filter(|e| matches!(e.kind, RiskEventKind::Veto { .. }))
        .count();

    if vetoes_last_10min >= 5 {
        tracing::error!(
            "ANOMALY: {vetoes_last_10min} vetoes in 10 minutes. \
            Auto-reverting to last safe config."
        );
        return Some(last_safe.clone());
    }

    None
}
```

## Risk Veto Logging

Every veto is logged with full context for later analysis:

```json
{
    "timestamp": "2026-08-05T14:30:00Z",
    "trigger_id": 42,
    "rule": "MaxDrawdownLockRule",
    "reason": "Cannot sell SOL at $95.20: below 50% of entry $210.00",
    "context": {
        "symbol": "SOL",
        "action": "Sell",
        "current_price": 95.20,
        "entry_price": 210.00,
        "drawdown_pct": 54.67,
        "threshold_pct": 50.0
    }
}
```

This data feeds into the Performance Reviewer to identify if risk rules are
too strict or too lenient.

## Parameter Change Audit Trail

Every risk parameter change is logged immutably:

```sql
CREATE TABLE IF NOT EXISTS risk_param_changes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL,
    user_id INTEGER NOT NULL,
    param_name TEXT NOT NULL,
    old_value TEXT NOT NULL,
    new_value TEXT NOT NULL,
    risk_tier TEXT NOT NULL,       -- 'conservative', 'moderate', 'aggressive'
    validation_result TEXT NOT NULL, -- 'passed', 'warning_shown', 'cooldown_applied'
    change_count_24h INTEGER NOT NULL
);
```

---

## Mode Gate Rule — `TradingModeGateRule`

**Priority:** Second (after Circuit Breaker) — checked before any other rule.

```
IF trading_mode == Accumulate:
    IF action == Sell AND NOT emergency_thesis_break:
        → VETO
        → Reason: "Accumulate mode: sell triggers are disabled.
                   Switch to Swing mode to enable two-sided trading."

    IF action == Buy:
        → Tiered deployment check:
            deployed_fraction = total_deployed / initial_capital
            discount_pct = (current_price - 30d_avg) / 30d_avg * 100
            → tier = find tier where discount_pct <= tier.threshold
            → max_allowed_fraction = sum of tier fractions up to this tier
            → IF deployed_fraction + position_fraction > max_allowed_fraction:
                → VETO
                → Reason: "Accumulate mode tier limit: cannot deploy more
                           than {max}% of capital at {discount}% discount.
                           Wait for a deeper discount or switch to Swing."
```

### Mode Change Validation

```rust
fn validate_mode_change(current: TradingMode, requested: TradingMode) -> Result<Vec<ModeSideEffect>, ModeError> {
    match (current, requested) {
        (TradingMode::Swing, TradingMode::Accumulate) => {
            // Pending SELL triggers must be cancelled (user confirms first)
            Ok(vec![ModeSideEffect::CancelPendingSellTriggers])
        }
        (TradingMode::Accumulate, TradingMode::Swing) => {
            // No side effects — buy triggers remain valid
            Ok(vec![])
        }
        _ => Ok(vec![]),
    }
}
```

Mode changes are logged in `audit_log` with user ID and timestamp.
