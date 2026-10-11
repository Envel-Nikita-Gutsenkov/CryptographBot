# Logging & Analytics Export

The system maintains a comprehensive audit trail and provides multiple export
formats for analysis. All logs are pre-processed — not raw data dumps.

---

## Log Types

### 1. Trading Journal (`trading_journal`)

One entry per AI cycle. Contains:
- Full market context at decision time
- Portfolio state before decisions
- AI's complete reasoning chain
- All triggers set, cancelled, or modified
- Active memos and strategy directive

**Retention:** Last 1000 entries in SQLite (~90 days at 6h cycles).
Older entries are pruned. Full archive can be exported.

### 2. Trade Execution Log (`virtual_trades`)

One entry per executed trade. Contains:
- Symbol, side, amount, price
- Fee and slippage breakdown
- Trigger reference
- AI cycle attribution
- Risk check result

**Retention:** All trades kept indefinitely (small data).

### 3. Performance Attribution (`performance_log`)

Links trade outcomes to AI decisions. Contains:
- Which AI models made the decision
- Strategy label
- Entry and exit prices
- Realized P&L
- Holding duration
- Veto information

**Retention:** All entries kept indefinitely.

### 4. Risk Veto Log (`risk_vetoes`)

One entry per vetoed trade. Contains:
- Which risk rule triggered
- Full context (prices, amounts, thresholds)
- AI's original intent

**Retention:** Last 500 entries.

---

## Export Formats

### JSON Export (Machine-Readable)

```json
{
    "export_type": "trading_journal",
    "generated_at": "2026-08-05T15:00:00Z",
    "period": {
        "from": "2026-07-06T00:00:00Z",
        "to": "2026-08-05T15:00:00Z"
    },
    "summary": {
        "total_cycles": 60,
        "total_trades": 12,
        "realized_pnl_usd": 45.20,
        "win_rate_pct": 58.3,
        "profit_factor": 1.40,
        "sharpe_ratio": 1.15,
        "max_drawdown_pct": -12.3,
        "total_fees_usd": 3.80,
        "total_slippage_usd": 1.90
    },
    "entries": [
        {
            "cycle_id": "2026-08-05T14:00:00Z-abc123",
            "timestamp": "2026-08-05T14:00:00Z",
            "models_used": ["qwen-2.5-72b (groq)", "llama-3.1-70b (openrouter)", "gemini-2.5-pro (critic)"],
            "market_snapshot": { ... },
            "portfolio_before": { ... },
            "ai_reasoning": "Cancelled ETH buy at $2400...",
            "triggers_set": [ ... ],
            "triggers_cancelled": [42],
            "active_memos": ["Focus on BTC and SOL..."],
            "strategy_directive": "Focus on accumulation...",
            "macro_context": "Accumulation phase..."
        }
    ]
}
```

### CSV Export (Spreadsheet)

```
cycle_id,timestamp,symbol,action,amount_usd,price_usd,fee_usd,slippage_usd,pnl_usd,ai_model,strategy
abc123,2026-08-05T14:00,BTC,Buy,100.00,65432.10,0.10,0.05,,qwen-2.5-72b,Accumulation
def456,2026-08-03T08:00,SOL,Sell,50.00,142.30,0.05,0.02,12.50,llama-3.1-70b,Accumulation
```

### Formatted Text Report (Human-Readable)

See `user-commands.md` → Export Performance Analytics section for the
formatted report template.

---

## Analytics Metrics

### Portfolio Metrics

| Metric | Formula | Description |
|---|---|---|
| Total Return | `(current_value - initial_capital) / initial_capital × 100` | Overall portfolio performance |
| Realized P&L | `Σ(sell_net_value - cost_basis_portion)` | Profit/loss from closed positions |
| Unrealized P&L | `Σ(holding × current_price - remaining_cost_basis)` | Paper profit/loss |
| Max Drawdown | `max(peak_value - trough_value) / peak_value × 100` | Largest peak-to-trough decline |
| Sharpe Ratio | `(mean_daily_return - risk_free_rate) / stddev_daily_return × √365` | Risk-adjusted return |

### Trading Metrics

| Metric | Formula | Description |
|---|---|---|
| Win Rate | `winning_trades / total_trades × 100` | % of profitable trades |
| Profit Factor | `gross_profit / gross_loss` | >1.0 = profitable system |
| Avg Win/Loss | `avg(win_amount) / avg(loss_amount)` | Reward-to-risk ratio |
| Expectancy | `(win_rate × avg_win) - ((1-win_rate) × avg_loss)` | Expected value per trade |
| Avg Hold Time | `avg(exit_time - entry_time)` | Average trade duration |

### AI Performance Metrics

| Metric | Description |
|---|---|
| Triggers Set vs Executed | Is the AI setting too many triggers that never fire? |
| Triggers Cancelled vs Expired | Is the AI changing its mind too often? |
| Risk Vetos per Cycle | Is the AI frequently hitting risk limits? |
| Model Attribution | Which AI models produce the best P&L? |
| Strategy Adherence | Is the AI following the user's strategy directive? |
| Hallucination Rate | How often does the Critic reject a Generator for price hallucination? |

---

## Decision Attribution System

Every trade is linked to the AI cycle that created its trigger. This enables
answering questions like:

- "Which AI model combination makes the best decisions?"
- "Does the AI perform better in bull or bear markets?"
- "Is the AI improving over time?"
- "Which strategy directive produces the best results?"

### Attribution Query Examples

```sql
-- Best performing AI model (generator)
SELECT ai_model_generators, AVG(pnl_pct) as avg_pnl, COUNT(*) as trades
FROM performance_log
WHERE pnl_realized_usd IS NOT NULL
GROUP BY ai_model_generators
ORDER BY avg_pnl DESC;

-- Performance by strategy
SELECT strategy_label, SUM(pnl_realized_usd) as total_pnl, COUNT(*) as trades
FROM performance_log
WHERE pnl_realized_usd IS NOT NULL
GROUP BY strategy_label;

-- AI improvement over time (by week)
SELECT strftime('%Y-%W', executed_at) as week, AVG(pnl_pct) as avg_pnl
FROM virtual_trades t
JOIN performance_log p ON t.id = p.trade_id
WHERE t.side = 'Sell'
GROUP BY week
ORDER BY week;

-- Risk rule veto frequency
SELECT rule, COUNT(*) as veto_count
FROM risk_vetoes
WHERE timestamp > datetime('now', '-30 days')
GROUP BY rule
ORDER BY veto_count DESC;
```

---

## Export File Management

### File Format

- Files are sent as Telegram Document attachments
- JSON exports use `.json` extension
- CSV exports use `.csv` extension
- Formatted reports sent as Telegram messages (text)
- Charts sent as PNG images

### Size Limits

- Max export size: 10 MB (Telegram bot file size limit: 50 MB)
- If journal exceeds 10 MB, export is split into 30-day chunks
- User is prompted: "Journal is large. Export last 30 days or all?"

### File Naming Convention

```
cryptograph_trading_journal_2026-08-05.json
cryptograph_trading_journal_2026-08-05.csv
cryptograph_performance_report_2026-08-05.txt
cryptograph_pnl_chart_2026-08-05.png
```

---

## Log Rotation & Cleanup

```rust
/// Prune old journal entries to prevent unbounded growth.
async fn prune_journal(sqlite: &SqliteStore) {
    let max_entries = 1000;
    let count = sqlite.count_journal_entries().await;

    if count > max_entries {
        let to_delete = count - max_entries;
        sqlite.delete_oldest_journal_entries(to_delete).await;
        tracing::info!("Pruned {to_delete} old journal entries (keeping {max_entries})");
    }
}
```

---

## Analytics Dashboard (Future)

A future web dashboard could visualize:

- Equity curve (portfolio value over time)
- Drawdown chart
- Monthly returns heatmap
- Trade distribution by asset
- Win rate by day of week / hour of day
- AI model performance comparison
