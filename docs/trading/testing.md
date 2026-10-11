# Testing Framework — Paper Trading Sandbox

The testing strategy operates at four levels, from fully synthetic to fully live.

---

## Testing Pyramid

```
           ┌──────────────┐
           │  LIVE MARKET  │  ← Level 4: Real prices, real AI, no execution
           │  SHADOW MODE  │
           ├──────────────┤
           │   HISTORICAL  │  ← Level 3: Real historical data, full AI cycles
           │   BACKTEST    │
           ├──────────────┤
           │   MOCK CLI    │  ← Level 2: Synthetic scenarios, deterministic
           │   SCENARIOS   │
           ├──────────────┤
           │   UNIT TESTS  │  ← Level 1: Individual functions, risk rules
           └──────────────┘
```

---

## Level 1 — Unit Tests

Every pure function is tested in isolation.

### Test Categories

| Module | What's Tested | File |
|---|---|---|
| `trading/risk.rs` | All 12 risk rules: approve, veto, edge cases | `tests/trading/risk_tests.rs` |
| `trading/triggers.rs` | Trigger evaluation: PriceBelow, PriceAbove, TrailingStop, TakeProfit, StopLoss, TimeDecay | `tests/trading/trigger_tests.rs` |
| `trading/fees.rs` | Fee calculation, slippage estimation, break-even math | `tests/trading/fee_tests.rs` |
| `trading/engine.rs` | Execution flow with mock prices | `tests/trading/engine_tests.rs` |
| `trading/ai_brain.rs` | Tool call parsing, JSON output validation | `tests/trading/ai_brain_tests.rs` |
| `trading/risk_params.rs` | Parameter validation, safe ranges, hazard detection | `tests/trading/risk_params_tests.rs` |
| `crypto/consensus.rs` | Existing + trading-specific outlier scenarios | (existing) |
| `db/sqlite.rs` | Virtual trade CRUD, journal persistence | `tests/trading/db_tests.rs` |

### Example: Risk Rule Unit Test

```rust
#[tokio::test]
async fn test_max_drawdown_lock_vetoes_sell_below_half_entry() {
    let mut portfolio = VirtualPortfolio::new(1000.0);
    // Simulate buying SOL at $200
    portfolio.holdings.insert("SOL".into(), VirtualHolding {
        amount: 1.0,
        avg_cost_basis: 200.0,
        total_cost_usd: 200.0,
        first_purchase_at: Utc::now() - Duration::hours(48),
        last_purchase_at: Utc::now() - Duration::hours(48),
        purchase_count: 1,
    });

    let trigger = ActiveTrigger {
        id: 1,
        symbol: "SOL".into(),
        condition: TriggerCondition::PriceBelow,
        target_price: 90.0,
        action: TriggerAction::Sell,
        amount_usd: 90.0,
        // ... other fields
    };

    let risk_params = RiskParameters::default(); // max_drawdown_sell_pct = 0.50
    let prices = HashMap::from([("SOL".into(), 90.0)]);

    let result = evaluate_trade_risk(
        &trigger, &portfolio, 90.0, &prices, &[], &risk_params, None, None
    );

    assert!(matches!(result, RiskResult::Vetoed { rule, .. } if rule == "MaxDrawdownLockRule"));
}

#[tokio::test]
async fn test_max_drawdown_lock_allows_sell_above_half_entry() {
    let mut portfolio = VirtualPortfolio::new(1000.0);
    portfolio.holdings.insert("SOL".into(), VirtualHolding {
        amount: 1.0,
        avg_cost_basis: 200.0,
        total_cost_usd: 200.0,
        first_purchase_at: Utc::now() - Duration::hours(48),
        last_purchase_at: Utc::now() - Duration::hours(48),
        purchase_count: 1,
    });

    let trigger = ActiveTrigger {
        id: 1,
        symbol: "SOL".into(),
        condition: TriggerCondition::PriceAbove,
        target_price: 150.0,
        action: TriggerAction::Sell,
        amount_usd: 150.0,
        // ...
    };

    let risk_params = RiskParameters::default();
    let prices = HashMap::from([("SOL".into(), 150.0)]);

    let result = evaluate_trade_risk(
        &trigger, &portfolio, 150.0, &prices, &[], &risk_params, None, None
    );

    assert!(matches!(result, RiskResult::Approved));
}
```

---

## Level 2 — Mock CLI Scenarios (Deterministic)

A CLI mode that runs the **full trading pipeline** against **synthetic,
deterministic market data** — no external APIs, no AI calls. Designed for
rapid iteration and CI.

### CLI Command

```bash
cargo run --bin cryptograph -- trade-demo --scenario trending_bull
cargo run --bin cryptograph -- trade-demo --scenario flash_crash
cargo run --bin cryptograph -- trade-demo --scenario sideways_chop
cargo run --bin cryptograph -- trade-demo --scenario all
```

### Scenario Definitions

Scenarios are defined as JSON files in `tests/scenarios/`:

```json
{
    "name": "trending_bull",
    "description": "BTC trends up from $60k to $70k over 30 days with small pullbacks. ETH and SOL follow with higher beta.",
    "duration_days": 30,
    "initial_capital_usd": 1000,
    "tick_interval_minutes": 60,
    "ai_cycle_interval_hours": 6,
    "assets": {
        "BTC": {
            "start_price": 60000,
            "price_path": [
                {"day": 0, "price": 60000},
                {"day": 5, "price": 62000},
                {"day": 10, "price": 61000},
                {"day": 15, "price": 65000},
                {"day": 20, "price": 64000},
                {"day": 25, "price": 68000},
                {"day": 30, "price": 70000}
            ],
            "volatility": 0.02,
            "volume_24h": 30000000000
        },
        "ETH": {
            "start_price": 2400,
            "beta_to_btc": 1.3,
            "volatility": 0.03,
            "volume_24h": 15000000000
        },
        "SOL": {
            "start_price": 120,
            "beta_to_btc": 1.8,
            "volatility": 0.04,
            "volume_24h": 3000000000
        }
    },
    "ai_behavior": "mock",  // Uses mock AI that follows predefined decisions
    "expected_outcome": {
        "min_roi_pct": 5.0,
        "max_roi_pct": 25.0,
        "min_win_rate": 50.0,
        "max_drawdown_pct": -20.0
    }
}
```

### Predefined Scenarios

| Scenario | Description | Tests |
|---|---|---|
| `trending_bull` | Steady uptrend with small dips | AI should accumulate on dips, ride trend |
| `trending_bear` | Steady downtrend with dead cat bounces | AI should stay in USD or cut losses early |
| `flash_crash` | Sudden -20% BTC drop then quick recovery | Circuit breaker must activate, AI must not panic-sell |
| `sideways_chop` | Range-bound market for 30 days | AI should not overtrade; fees would kill |
| `pump_and_dump` | +50% pump over 3 days, then -60% dump | AI should take profit, not chase the pump |
| `black_swan` | -40% in 2 hours, no recovery | Risk rules must limit damage to max drawdown |
| `correlated_crash` | All assets drop together | Concentration rules must prevent overexposure |
| `low_liquidity` | Volume dries up, wide spreads | Liquidity and slippage rules must block trades |
| `whipsaw` | Repeated +5% / -5% swings | Wash sale prevention + cooldowns must activate |
| `perfect_trend` | Clean +30% uptrend | AI should capture most of the move |

### Mock AI for Scenarios

In mock mode, instead of calling real AI APIs, a `MockAI` reads predefined
decisions from the scenario file:

```json
{
    "mock_ai_decisions": [
        {
            "cycle": 1,
            "hour": 6,
            "actions": [
                {
                    "tool": "set_trigger",
                    "params": {
                        "symbol": "BTC",
                        "condition": "PriceBelow",
                        "target_price": 59500,
                        "action": "Buy",
                        "amount_usd": 100,
                        "reasoning": "Buying the dip near $60k support"
                    }
                }
            ]
        },
        {
            "cycle": 8,
            "hour": 48,
            "actions": [
                {
                    "tool": "set_trigger",
                    "params": {
                        "symbol": "BTC",
                        "condition": "TakeProfit",
                        "target_price": 66000,
                        "action": "Sell",
                        "amount_usd": 100,
                        "take_profit_stages": [
                            {"gain_pct": 10.0, "sell_fraction": 0.5},
                            {"gain_pct": 20.0, "sell_fraction": 0.5}
                        ],
                        "reasoning": "Taking profit as BTC breaks $66k"
                    }
                }
            ]
        }
    ]
}
```

This allows testing the Execution Engine and Risk Manager deterministically
without API costs or rate limits.

### CLI Output Example

```
═══════════════════════════════════════════════════
  CRYPTOGRAPH TRADING DEMO
  Scenario: trending_bull (30 days, 60-min ticks)
═══════════════════════════════════════════════════

Day  1 | BTC $60,000 | Portfolio: $1,000.00
Day  1 | AI Cycle #1: Set BUY trigger BTC @ $59,500 ($100)
Day  2 | BTC $59,450 → TRIGGER FIRED: Buy 0.00168 BTC
Day  5 | BTC $62,000 | Portfolio: $1,004.20 (+0.42%)
Day  5 | AI Cycle #2: Set TAKEPROFIT BTC @ $66,000 (2-stage)
Day 10 | BTC $61,000 | Waiting...
Day 15 | BTC $65,000 | Portfolio: $1,008.40 (+0.84%)
...
Day 25 | BTC $67,800 → TAKEPROFIT FIRED: Sell 50% @ +14.2%
Day 30 | BTC $70,000 → TAKEPROFIT FIRED: Sell 50% @ +17.9%

═══════════════════════════════════════════════════
  RESULTS
═══════════════════════════════════════════════════
  Initial Capital:    $1,000.00
  Final Value:        $1,082.50
  ROI:                +8.25%

  Trades:             4
  Win Rate:           75.0%
  Profit Factor:      2.15
  Max Drawdown:       -2.1%

  Fees Paid:          $2.40
  Slippage Cost:      $1.10
  Net Profit:         +$78.00

  Risk Vetos:         0
  Circuit Breakers:   0

  Verdict:            ✅ PASSED (ROI 8.25% > min 5.0%)
═══════════════════════════════════════════════════
```

---

## Level 3 — Historical Backtest

Runs the full AI pipeline (real AI calls) against **real historical market
data**. This is the most realistic test before live paper trading.

### CLI Command

```bash
cargo run --bin cryptograph -- trade-backtest \
    --from 2024-01-01 \
    --to 2024-03-31 \
    --capital 1000 \
    --assets BTC,ETH,SOL \
    --ai-cycle 6h \
    --output backtest_results/
```

### Data Source

Uses existing CMC historical data endpoint (`/v2/cryptocurrency/quotes/historical`)
or cached data from SQLite `price_history` table.

### Backtest Flow

```
1. Load historical price data for period
2. Initialize VirtualPortfolio with start capital
3. For each AI cycle timestamp:
   a. Build market snapshot from historical data at that timestamp
   b. Call real AI pipeline (Generator→Critic→Challenger→Arbiter)
   c. Execute AI decisions (set/cancel triggers)
4. Run Execution Engine ticks between AI cycles
5. At each tick: evaluate triggers against historical prices
6. Record all trades, journal entries, risk vetos
7. Generate final report
```

### Backtest Report Output

```
═══════════════════════════════════════════════════
  BACKTEST REPORT
  Period: 2024-01-01 → 2024-03-31 (90 days)
  AI Cycles: 360 (every 6h)
═══════════════════════════════════════════════════

RETURNS
  Strategy Return:     +34.2%
  Buy & Hold BTC:      +62.1%
  Buy & Hold ETH:      +48.3%
  DCA Benchmark:       +28.7%

  Alpha vs DCA:        +5.5%
  Alpha vs BTC:        -27.9%  ← Strategy underperformed BTC

RISK METRICS
  Strategy Max DD:     -12.3%
  BTC Max DD:          -21.8%
  Risk-Adjusted (Sortino): 1.85 vs BTC 1.42

TRADING
  Total Trades:        28
  Win Rate:            64.3%
  Avg Hold Time:       4.2 days
  Total Fees:          $15.40

AI PERFORMANCE
  Hallucinations:      3 (0.8% of cycles)
  Risk Vetos:          12
  Triggers Set:        85
  Triggers Executed:   28 (32.9% fill rate)

DEBATE STATISTICS
  Plans REJECTED:      8 (2.2% of cycles)
  Plans MODIFIED:      42 (11.7% of cycles)
  Plans APPROVED:      310 (86.1% of cycles)
  Avg Confidence:      0.78

═══════════════════════════════════════════════════
```

### Backtest vs Benchmark Comparison

The backtest automatically compares against:

| Benchmark | Description |
|---|---|
| Buy & Hold (per asset) | What if you just bought and held? |
| DCA (equal-weight) | Dollar-cost averaging into all assets |
| 60/40 Portfolio | 60% BTC, 40% USD |
| Perfect Oracle | Buy every local bottom, sell every local top (theoretical max) |

---

## Level 4 — Live Market Shadow Mode

The AI runs with **real-time market data** and **real AI calls**, but trades
are **purely virtual**. This is the final validation before considering real
money.

### CLI Command

```bash
cargo run --bin cryptograph -- trade-live-shadow \
    --capital 1000 \
    --assets BTC,ETH,SOL \
    --ai-cycle 6h \
    --duration-days 30
```

### What's Real vs Virtual

| Component | Live Shadow Mode |
|---|---|
| Market prices | ✅ Real (consensus from CMC/CoinGecko/Binance) |
| AI models | ✅ Real (Gemini, Groq, OpenRouter) |
| AI decisions | ✅ Real (full pipeline with debate) |
| Trade execution | ❌ Virtual (paper trades only) |
| Risk management | ✅ Real (rules enforced) |
| Portfolio tracking | ❌ Virtual (no real money) |
| Logging/analytics | ✅ Real (full journal) |

### Live Shadow Dashboard

During live shadow mode, a periodic status is printed to console (or sent to
Telegram if configured):

```
─────────────────────────────────────────────────
  LIVE SHADOW — Day 7 of 30
  Aug 5, 2026 14:00 UTC
─────────────────────────────────────────────────
  Portfolio:   $1,023.40 (+2.34%)
  Available:   $650.00
  Holdings:    BTC $163.58 | SOL $209.82
  Triggers:    3 active
  Trades:      5 total | 3 wins | 2 losses
  Risk Vetos:  1 (MaxPositionSize)
─────────────────────────────────────────────────
  Last AI Cycle (12:00 UTC):
  ✅ Set BUY SOL @ $130 ($50)
  ❌ Cancelled ETH BUY (price moved away)
  Confidence: 82% | Debate: 6/7 defended
─────────────────────────────────────────────────
```

---

## CI/CD Integration

```yaml
# .github/workflows/trading-tests.yml
name: Trading Tests

on:
  push:
    branches: [feature/paper-trading-sandbox]
  pull_request:

jobs:
  unit-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo test --lib trading -- --nocapture

  scenario-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo run --bin cryptograph -- trade-demo --scenario all --ci

  risk-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo test --test trading_risk_tests

  backtest-smoke:
    runs-on: ubuntu-latest
    needs: [unit-tests, scenario-tests]
    steps:
      - uses: actions/checkout@v4
      - run: |
          cargo run --bin cryptograph -- trade-backtest \
            --from 2026-07-01 --to 2026-07-07 \
            --capital 1000 --assets BTC \
            --ci-smoke
```

---

## Test Coverage Targets

| Module | Target Coverage | Critical Paths |
|---|---|---|
| `trading/risk.rs` | 100% | Every rule + every edge case |
| `trading/triggers.rs` | 100% | Every condition type + boundary values |
| `trading/fees.rs` | 95% | Fee math, slippage model, break-even |
| `trading/engine.rs` | 90% | Full execution flow, error paths |
| `trading/ai_brain.rs` | 85% | Tool parsing, prompt construction |
| `trading/risk_params.rs` | 100% | Every parameter range boundary + hazard |
| `trading/moe_validator.rs` | 80% | Critic logic, debate flow |

---

## Fuzzing & Edge Cases

For untrusted inputs (AI-generated parameters), fuzz testing is applied:

```rust
#[test]
fn fuzz_trigger_parameters() {
    // Generate random trigger parameters
    // Verify: no crash, no panic, always returns valid Result
    for _ in 0..10000 {
        let symbol = random_symbol();
        let price = random_f64(0.0, 1_000_000.0);
        let amount = random_f64(-1000.0, 1_000_000.0);
        let condition = random_condition();

        let result = validate_set_trigger(&symbol, price, amount, &condition);
        // Must not panic — must return Ok or Err
        assert!(result.is_ok() || result.is_err());
    }
}

#[test]
fn fuzz_risk_parameters() {
    for _ in 0..10000 {
        let params = RiskParameters {
            max_position_pct: random_f64(-1.0, 2.0),
            max_concentration_pct: random_f64(-1.0, 2.0),
            // ... randomize all fields
        };

        // Validation must not panic
        let _ = validate_all_risk_params(&params);
    }
}
```

---

## Regression Test Suite

After any code change, a fast regression suite runs:

```bash
# Fast regression (< 30 seconds)
cargo test trading:: --lib

# Scenario regression (< 2 minutes)
cargo run --bin cryptograph -- trade-demo --scenario quick_regression

# Full regression (< 10 minutes, runs on PR)
cargo test trading::
cargo run --bin cryptograph -- trade-demo --scenario all
```

---

## Real-World Test Checklist

Before considering real-money trading, ALL of these must pass:

- [ ] All unit tests pass (100% risk module coverage)
- [ ] All 10 scenarios pass with expected outcomes
- [ ] 30-day historical backtest shows positive alpha vs DCA
- [ ] 7-day live shadow mode: no crashes, no data corruptions
- [ ] 30-day live shadow mode: win rate > 50%, profit factor > 1.0
- [ ] 30-day live shadow mode: max drawdown < risk limit
- [ ] 30-day live shadow mode: zero unhandled errors in logs
- [ ] Circuit breaker tested: triggered correctly, released correctly
- [ ] Emergency stop tested: all triggers cancelled, engine paused
- [ ] Backup/restore tested: portfolio state survives restart
- [ ] Dead man's switch tested: triggers expire when AI offline
- [ ] All risk rules triggered at least once in live shadow
- [ ] Debate mode: at least 1 plan rejected by challenger
- [ ] Performance reviewer: weekly report generated and accurate
- [ ] Export tested: JSON, CSV, text all valid and complete
