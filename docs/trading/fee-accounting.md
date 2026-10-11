# Fee Accounting & Real P&L Simulation

To make paper trading realistic, every virtual trade simulates the costs that
would occur in a real trading environment.

---

## Fee Model

### Centralized Exchange (CEX) Model — Default

Simulates trading on a Tier-1 exchange (Binance/Bybit/Coinbase):

| Fee Type | Rate | Notes |
|---|---|---|
| Maker fee | 0.10% | Applied when trigger is PriceBelow/PriceAbove (limit order) |
| Taker fee | 0.10% | Applied when trigger is StopLoss/TrailingStop (market order) |
| Withdrawal fee | Not simulated | Paper trading doesn't move assets between wallets |

For simplicity in v1, all trades use a flat **0.10%** fee.

```rust
const DEFAULT_FEE_PCT: f64 = 0.001; // 0.10%

fn calculate_fee(amount_usd: f64, fee_pct: f64) -> f64 {
    (amount_usd * fee_pct * 100.0).round() / 100.0 // Round to cents
}
```

### Decentralized Exchange (DEX) Model — Future

For on-chain swap simulation (Phase 9):

| Cost Component | Typical Range | Notes |
|---|---|---|
| DEX swap fee | 0.05%–0.30% | Uniswap: 0.30%, PancakeSwap: 0.25% |
| Gas fee (ETH) | $2–$50+ | Varies by network congestion |
| Gas fee (SOL) | $0.0001–$0.01 | Negligible for most trades |
| Slippage | 0.1%–2%+ | Depends on pool depth and trade size |
| MEV protection | Variable | Flashbots bundle tip |

---

## Slippage Simulation

Slippage is estimated based on trade size relative to 24h volume:

```rust
fn simulate_slippage(symbol: &str, amount_usd: f64, volume_24h: f64) -> f64 {
    if volume_24h <= 0.0 {
        return 0.0;
    }

    // Base model: slippage proportional to (trade_size / daily_volume)
    let volume_ratio = amount_usd / volume_24h;

    // Empirical formula fits typical crypto market behavior:
    // - 0.01% of daily volume → ~0.02% slippage
    // - 0.10% of daily volume → ~0.15% slippage
    // - 1.00% of daily volume → ~1.20% slippage
    let slippage_pct = 0.02 + (volume_ratio * 100.0).powf(0.7) * 1.15;

    // Add noise (±20%) to simulate real market randomness
    let noise = 1.0 + (rand::random::<f64>() - 0.5) * 0.4;
    let final_slippage = slippage_pct * noise;

    // Cap at reasonable maximum
    final_slippage.min(5.0)
}
```

**Slippage is always negative** — it reduces the net value of every trade.

For BUY trades: `execution_price = consensus_price * (1.0 + slippage_pct)`
For SELL trades: `execution_price = consensus_price * (1.0 - slippage_pct)`

---

## Real P&L Calculation

### Per-Trade P&L

```rust
fn calculate_trade_pnl(trade: &VirtualTrade, entry_trade: &VirtualTrade) -> TradePnl {
    match trade.side {
        TradeSide::Sell => {
            // P&L = sell_net_value - proportional_cost_of_sold_amount
            let sell_fraction = trade.amount_crypto / get_holding_amount_at_time(&trade.symbol, trade.executed_at);
            let cost_basis_portion = entry_trade.net_value_usd * sell_fraction;

            TradePnl {
                realized_usd: trade.net_value_usd - cost_basis_portion,
                realized_pct: (trade.net_value_usd - cost_basis_portion) / cost_basis_portion * 100.0,
                fees_paid: trade.fee_usd,
                slippage_cost: trade.slippage_usd,
            }
        }
        TradeSide::Buy => {
            TradePnl {
                realized_usd: 0.0, // P&L only realized on sell
                realized_pct: 0.0,
                fees_paid: trade.fee_usd,
                slippage_cost: trade.slippage_usd,
            }
        }
    }
}
```

### Portfolio-Level P&L

```rust
fn portfolio_pnl_summary(
    portfolio: &VirtualPortfolio,
    prices: &HashMap<String, f64>,
    trades: &[VirtualTrade],
) -> PortfolioPnlSummary {
    let total_value = portfolio.total_value_usd(prices);

    // Realized P&L: sum of all sell_trade.net_value - proportional_cost_basis
    let realized_pnl: f64 = calculate_realized_pnl(trades);

    // Unrealized P&L: current_holdings_value - remaining_cost_basis
    let unrealized_pnl = portfolio.unrealized_pnl_usd(prices);

    // Total fees paid
    let total_fees: f64 = trades.iter().map(|t| t.fee_usd).sum();

    // Total slippage cost
    let total_slippage: f64 = trades.iter().map(|t| t.slippage_usd).sum();

    PortfolioPnlSummary {
        initial_capital: portfolio.initial_capital_usd,
        current_value: total_value,
        absolute_return: total_value - portfolio.initial_capital_usd,
        roi_pct: (total_value - portfolio.initial_capital_usd) / portfolio.initial_capital_usd * 100.0,
        realized_pnl,
        unrealized_pnl,
        total_fees_paid: total_fees,
        total_slippage_paid: total_slippage,
        net_return_after_costs: realized_pnl + unrealized_pnl - total_fees - total_slippage,
    }
}
```

---

## Fee Impact Visualization

The system should make fee impact visible to the AI and user:

```
┌─────────────────────────────────────────┐
│           TRADE SUMMARY                  │
├─────────────────────────────────────────┤
│  Gross Value:              $100.00       │
│  Exchange Fee (0.10%):    -$0.10        │
│  Slippage (0.05%):        -$0.05        │
│  ─────────────────────────────────────  │
│  Net Value:                $99.85       │
│                                         │
│  Entry Cost Basis:         $99.85       │
│  Required Gain to Break Even: +0.15%    │
└─────────────────────────────────────────┘
```

This teaches the AI that frequent trading erodes returns through fees and
slippage — a critical real-world lesson.

---

## Fee Configuration

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeConfig {
    /// Fee percentage (0.0–1.0). Default: 0.001 (0.10%).
    pub fee_pct: f64,

    /// Whether to simulate slippage. Default: true.
    pub simulate_slippage: bool,

    /// Slippage model: "volume_based" or "fixed".
    pub slippage_model: SlippageModel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SlippageModel {
    /// Slippage based on trade size / 24h volume ratio.
    VolumeBased,
    /// Fixed slippage percentage for all trades.
    Fixed(f64),
}
```

---

## Break-Even Analysis

After fees and slippage, the AI needs a minimum gain just to break even:

```rust
fn break_even_pct(fee_pct: f64, avg_slippage_pct: f64) -> f64 {
    // Round-trip: buy (fee + slippage) + sell (fee + slippage)
    let buy_cost_pct = fee_pct + avg_slippage_pct;
    let sell_cost_pct = fee_pct + avg_slippage_pct;

    // Approximate: need gain > total costs
    buy_cost_pct + sell_cost_pct
}

// With 0.10% fee + 0.05% slippage:
// Break-even ≈ 0.30% per round-trip
```

This is reported to the AI so it understands the minimum viable trade size.
Micro-trades (e.g., $10) where fees eat all profit become self-evidently bad.
