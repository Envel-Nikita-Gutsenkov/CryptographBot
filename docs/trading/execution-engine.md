# Execution Engine — Algorithmic Trade Executor

The Execution Engine is a **deterministic Rust algorithm** that runs continuously.
It does not think — it only evaluates conditions and executes trades.

---

## Engine Lifecycle

```rust
pub async fn run_execution_engine(
    sqlite: Arc<SqliteStore>,
    store: Arc<Store>,
    crypto: Arc<CryptoClient>,
    ai_wake_tx: tokio::sync::mpsc::UnboundedSender<()>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(300)); // 5 min

    loop {
        interval.tick().await;

        // 1. Fetch consensus prices for all tracked assets
        let symbols = get_tracked_symbols(&store);
        let prices = fetch_consensus_prices(&crypto, &symbols).await;

        // 2. Load active triggers
        let triggers = store.read().active_triggers.clone();

        // 3. Evaluate each trigger
        for trigger in triggers {
            if trigger.status != TriggerStatus::Active {
                continue;
            }

            // Check expiry
            if let Some(expires) = trigger.expires_at {
                if Utc::now() > expires {
                    cancel_trigger(&store, trigger.id, "Expired").await;
                    continue;
                }
            }

            // Get current price
            let current_price = match prices.get(&trigger.symbol) {
                Some(p) => *p,
                None => continue, // skip if price unavailable
            };

            // Evaluate condition
            let should_fire = evaluate_condition(&trigger.condition, current_price);

            if should_fire {
                // Route through risk manager
                let portfolio = store.read().virtual_portfolio.clone();
                let risk_result = check_risk_rules(&trigger, &portfolio, current_price);

                match risk_result {
                    RiskResult::Approved => {
                        execute_virtual_trade(&store, &sqlite, &trigger, current_price).await;
                        // Wake up AI
                        let _ = ai_wake_tx.send(());
                    }
                    RiskResult::Vetoed { rule, reason } => {
                        mark_trigger_vetoed(&store, &sqlite, trigger.id, &rule, &reason).await;
                    }
                }
            }

            // Increment tick counter
            increment_trigger_ticks(&store, trigger.id).await;
        }
    }
}
```

---

## Trigger Evaluation Logic

```rust
fn evaluate_condition(condition: &TriggerCondition, current_price: f64) -> bool {
    match condition {
        TriggerCondition::PriceBelow => {
            current_price <= trigger.target_price
        }
        TriggerCondition::PriceAbove => {
            current_price >= trigger.target_price
        }
        TriggerCondition::StopLoss => {
            current_price <= trigger.target_price
        }
        TriggerCondition::TrailingStop { activate_after_pct, trail_step_pct, peak_price } => {
            let entry_price = get_entry_price_for_trigger(trigger);
            let gain_pct = (current_price - entry_price) / entry_price * 100.0;

            if gain_pct >= *activate_after_pct {
                // Activated: track peak and check trail
                let peak = peak_price.unwrap_or(current_price);
                let drawdown_pct = (peak - current_price) / peak * 100.0;
                drawdown_pct >= *trail_step_pct
            } else {
                false
            }
        }
        TriggerCondition::TakeProfit { stages } => {
            let entry_price = get_entry_price_for_trigger(trigger);
            let gain_pct = (current_price - entry_price) / entry_price * 100.0;

            stages.iter().any(|stage| {
                !stage.executed && gain_pct >= stage.gain_pct
            })
        }
        TriggerCondition::TimeDecay { .. } => {
            false // TimeDecay is checked at expiry, not price
        }
    }
}
```

---

## Virtual Trade Execution

```rust
async fn execute_virtual_trade(
    store: &Arc<Store>,
    sqlite: &Arc<SqliteStore>,
    trigger: &ActiveTrigger,
    execution_price: f64,
) {
    let mut portfolio = store.write();

    // Calculate amounts
    let fee_pct = 0.001; // 0.1% maker/taker fee
    let slippage = simulate_slippage(trigger.symbol.clone(), trigger.amount_usd);
    let fee_usd = trigger.amount_usd * fee_pct;
    let net_amount_usd = trigger.amount_usd - fee_usd - slippage;
    let amount_crypto = net_amount_usd / execution_price;

    match trigger.action {
        TriggerAction::Buy => {
            // Deduct from available USD
            portfolio.virtual_portfolio.available_usd -= trigger.amount_usd;

            // Add to holdings
            let holding = portfolio.virtual_portfolio.holdings
                .entry(trigger.symbol.clone())
                .or_insert_with(|| VirtualHolding::new());

            let new_total_cost = holding.total_cost_usd + net_amount_usd;
            let new_total_amount = holding.amount + amount_crypto;
            holding.amount = new_total_amount;
            holding.total_cost_usd = new_total_cost;
            holding.avg_cost_basis = new_total_cost / new_total_amount;
            holding.last_purchase_at = Utc::now();
            holding.purchase_count += 1;
        }
        TriggerAction::Sell => {
            let holding = portfolio.virtual_portfolio.holdings
                .get_mut(&trigger.symbol)
                .expect("Sell trigger for unowned asset");

            // Deduct from holdings
            holding.amount -= amount_crypto;
            holding.total_cost_usd -= holding.avg_cost_basis * amount_crypto;

            // Clean up zero holdings
            if holding.amount <= 1e-10 {
                let pnl = net_amount_usd - holding.total_cost_usd;
                portfolio.virtual_portfolio.holdings.remove(&trigger.symbol);
            }

            // Add to available USD
            portfolio.virtual_portfolio.available_usd += net_amount_usd;
        }
    }

    // Create trade record
    let trade = VirtualTrade {
        id: portfolio.virtual_portfolio.next_trade_id,
        symbol: trigger.symbol.clone(),
        side: trigger.action.clone(),
        amount_crypto,
        price_usd: execution_price,
        gross_value_usd: trigger.amount_usd,
        fee_usd,
        slippage_usd: slippage,
        net_value_usd: net_amount_usd,
        trigger_id: Some(trigger.id),
        ai_cycle_id: Some(trigger.created_by_ai_cycle.clone()),
        ai_reasoning_hash: hash_reasoning(&trigger.ai_reasoning),
        executed_at: Utc::now(),
        price_sources: vec![],
        risk_check: RiskCheckResult::Passed,
    };

    portfolio.virtual_portfolio.next_trade_id += 1;

    // Mark trigger as executed
    mark_trigger_executed_inner(&mut portfolio, trigger.id);

    // Persist to SQLite
    sqlite.save_virtual_trade(&trade).await;
    sqlite.mark_trigger_executed(trigger.id).await;
}
```

---

## Consensus Price Fetching

Reuses the existing `consensus::build_consensus()` from `crypto/consensus.rs`:

```rust
async fn fetch_consensus_prices(
    crypto: &Arc<CryptoClient>,
    symbols: &[String],
) -> HashMap<String, f64> {
    let mut prices = HashMap::new();

    for symbol in symbols {
        // Fetch from all providers in parallel (CMC, CoinGecko, CoinCap, etc.)
        let quotes = crypto.get_all_quotes(symbol).await;

        if let Some(consensus) = build_consensus(quotes) {
            if consensus.confidence >= 0.33 {
                // Require at least 1 valid source
                prices.insert(symbol.clone(), consensus.quote.price_usd);
            }
        }
    }

    prices
}
```

---

## Engine Configuration

```rust
pub struct EngineConfig {
    /// Tick interval in seconds (default: 300 = 5 minutes).
    pub tick_interval_secs: u64,

    /// Maximum concurrent triggers to evaluate per tick.
    pub max_triggers_per_tick: usize,

    /// Minimum consensus confidence to use a price (0.0–1.0).
    pub min_consensus_confidence: f64,

    /// If a price source hasn't updated in N seconds, skip it.
    pub price_staleness_secs: u64,

    /// Auto-cancel triggers older than N hours (dead man's switch).
    pub trigger_max_age_hours: f64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            tick_interval_secs: 300,
            max_triggers_per_tick: 50,
            min_consensus_confidence: 0.33,
            price_staleness_secs: 120,
            trigger_max_age_hours: 168.0, // 7 days
        }
    }
}
```

---

## Dead Man's Switch

If the AI brain process crashes or the bot goes offline, all active triggers
automatically expire after `trigger_max_age_hours` (default: 7 days). This
prevents stale triggers from executing when the AI is not monitoring them.

Additionally, the engine checks on every tick:

```rust
// If AI hasn't checked in for 2x the normal cycle interval, 
// escalate: log warning, notify user via Telegram
let last_ai_cycle = sqlite.get_last_ai_cycle_timestamp().await;
if Utc::now() - last_ai_cycle > Duration::hours(24) {
    tracing::error!("AI Brain has not checked in for 24+ hours!");
    // Send Telegram alert to user
    bot.send_message("⚠️ AI trading brain has been offline for 24+ hours. All triggers remain active but unmonitored.").await;
}
```

---

## Staleness Guard at Trigger Creation

See [vulnerability-response.md](vulnerability-response.md#v4-execution-latency-vs-market-volatility-high) for full analysis.

### Problem

The AI reads a market snapshot at Step 4 but places triggers at Step 20 (~110s later).
In volatile markets, the price can move 3-5% in that window, causing instant
execution at unintended prices.

### Solution

Every `set_trigger()` call re-fetches the current price and validates:

1. **Deviation check:** If current price deviates > `max_snapshot_deviation_pct` from snapshot, reject or auto-adjust
2. **Immediate-fire check:** If trigger would fire immediately (e.g., PriceBelow but price already below target), reject
3. **Staleness warning:** If snapshot > 30s old, warn but don't reject

### Per-Asset Deviation Thresholds

| Asset | Max Deviation |
|---|---|
| BTC | 0.5% |
| ETH | 1.0% |
| SOL | 2.0% |
| High-vol assets | 3.0% |

### Dynamic Trigger Expiration (Dead Man's Switch)

Default: `min(2 × AI cycle interval, 72h)`. Clamped to 4h–72h.

| AI Cycle | Expiration |
|---|---|
| 4h | 8h |
| 6h | 12h |
| 12h | 24h |
| 24h | 48h |

Escalating alerts: 12h no check-in → warning · 24h → all triggers cancelled.

See [vulnerability-response.md](vulnerability-response.md#v5-dead-mans-switch-duration-mismatch-medium).
