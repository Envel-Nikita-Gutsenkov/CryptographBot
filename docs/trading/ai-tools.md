# AI Toolset — Function Calling API

The AI Strategist interacts with the trading system exclusively through a
structured JSON function-calling interface. It cannot execute trades directly.

---

## Tool Definition Format

Each tool is described to the AI via the system prompt using OpenAI-compatible
function calling schema:

```json
{
    "name": "tool_name",
    "description": "What the tool does and when to use it",
    "parameters": {
        "type": "object",
        "properties": { ... },
        "required": [ ... ]
    }
}
```

The AI outputs one or more tool calls per cycle in a structured JSON format.
The system executes them sequentially and returns results.

---

## Core Tools

### `read_market_snapshot`

Get current consensus prices and market metrics for specified assets.

```json
{
    "name": "read_market_snapshot",
    "description": "Get consensus prices (median across CMC/CoinGecko/CoinCap/Binance) and market metrics for specified symbols. Always use this instead of guessing prices.",
    "parameters": {
        "type": "object",
        "properties": {
            "symbols": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Asset symbols to query, e.g. ['BTC', 'ETH', 'SOL']"
            }
        },
        "required": ["symbols"]
    }
}
```

**Returns:**
```json
{
    "BTC": {
        "price_usd": 65432.10,
        "change_1h_pct": 0.23,
        "change_24h_pct": -1.45,
        "change_7d_pct": 3.21,
        "change_30d_pct": -8.90,
        "ath_discount_pct": -12.34,
        "volume_24h_usd": 28500000000,
        "market_cap_usd": 1280000000000,
        "confidence": 1.0,
        "sources_used": ["binance", "coingecko", "coinmarketcap"],
        "anomaly_warning": null
    }
}
```

---

### `get_portfolio_state`

Get current virtual portfolio: balances, holdings, P&L.

```json
{
    "name": "get_portfolio_state",
    "description": "Get the current virtual portfolio state including available USD, holdings with cost basis, unrealized and realized P&L, and locked capital.",
    "parameters": {
        "type": "object",
        "properties": {},
        "required": []
    }
}
```

**Returns:**
```json
{
    "initial_capital_usd": 1000.00,
    "available_usd": 650.00,
    "locked_capital_usd": 0.00,
    "holdings": {
        "BTC": {
            "amount": 0.0025,
            "avg_cost_basis": 62000.00,
            "total_cost_usd": 155.00,
            "current_value_usd": 163.58,
            "unrealized_pnl_usd": 8.58,
            "unrealized_pnl_pct": 5.54
        }
    },
    "total_value_usd": 1013.58,
    "total_unrealized_pnl_usd": 8.58,
    "total_realized_pnl_usd": 5.00,
    "roi_pct": 1.36
}
```

---

### `set_trigger`

Place an algorithmic order that the Execution Engine will monitor.

```json
{
    "name": "set_trigger",
    "description": "Create an algorithmic trigger for the Execution Engine. The engine will monitor prices and execute when conditions are met. Triggers are subject to risk validation at execution time. You MUST explain your reasoning in the 'reasoning' field.",
    "parameters": {
        "type": "object",
        "properties": {
            "symbol": {
                "type": "string",
                "description": "Asset symbol, e.g. 'BTC'"
            },
            "condition": {
                "type": "string",
                "enum": ["PriceBelow", "PriceAbove", "StopLoss", "TrailingStop", "TakeProfit"]
            },
            "target_price": {
                "type": "number",
                "description": "Price threshold in USD that triggers execution"
            },
            "action": {
                "type": "string",
                "enum": ["Buy", "Sell"]
            },
            "amount_usd": {
                "type": "number",
                "description": "USD amount to trade. Must be ≤ available balance for buys, ≤ holding value for sells."
            },
            "expires_in_hours": {
                "type": "number",
                "description": "Auto-cancel if not triggered within N hours. Default: 168 (7 days). Max: 720 (30 days)."
            },
            "trailing_config": {
                "type": "object",
                "description": "Required if condition is 'TrailingStop'",
                "properties": {
                    "activate_after_pct": { "type": "number", "description": "Activate trailing after this % gain" },
                    "trail_step_pct": { "type": "number", "description": "Sell if price drops this % from peak" }
                }
            },
            "take_profit_stages": {
                "type": "array",
                "description": "Required if condition is 'TakeProfit'",
                "items": {
                    "type": "object",
                    "properties": {
                        "gain_pct": { "type": "number" },
                        "sell_fraction": { "type": "number", "description": "0.0–1.0 fraction to sell at this stage" }
                    }
                }
            },
            "reasoning": {
                "type": "string",
                "description": "YOUR REASONING: why you are setting this trigger. This is logged for performance analysis."
            }
        },
        "required": ["symbol", "condition", "target_price", "action", "amount_usd", "reasoning"]
    }
}
```

**Returns:**
```json
{
    "trigger_id": 42,
    "status": "Active",
    "risk_precheck": "Passed",
    "warning": null
}
```

---

### `cancel_trigger`

Remove a pending trigger before it executes.

```json
{
    "name": "cancel_trigger",
    "description": "Cancel a previously set trigger. Use when market conditions have changed and the trigger is no longer appropriate.",
    "parameters": {
        "type": "object",
        "properties": {
            "trigger_id": {
                "type": "integer",
                "description": "ID of the trigger to cancel"
            },
            "reason": {
                "type": "string",
                "description": "Why you are cancelling this trigger"
            }
        },
        "required": ["trigger_id", "reason"]
    }
}
```

**Returns:**
```json
{
    "trigger_id": 42,
    "cancelled": true,
    "previous_status": "Active"
}
```

---

### `list_active_triggers`

View all pending triggers.

```json
{
    "name": "list_active_triggers",
    "description": "List all active (pending) triggers currently monitored by the Execution Engine.",
    "parameters": {
        "type": "object",
        "properties": {},
        "required": []
    }
}
```

**Returns:**
```json
{
    "active_triggers": [
        {
            "id": 42,
            "symbol": "BTC",
            "condition": "PriceBelow",
            "target_price": 58000.00,
            "action": "Buy",
            "amount_usd": 100.00,
            "created_at": "2026-08-05T10:00:00Z",
            "expires_at": "2026-08-12T10:00:00Z",
            "ticks_survived": 24
        }
    ],
    "total_active": 1
}
```

---

### `review_trade_history`

Analyze past paper trades for performance insights.

```json
{
    "name": "review_trade_history",
    "description": "Get a summary of past trades: win rate, average P&L, best/worst trades, fees paid. Use this to learn from past decisions.",
    "parameters": {
        "type": "object",
        "properties": {
            "days": {
                "type": "integer",
                "description": "How many days of history to review. Default: 30."
            },
            "symbol": {
                "type": "string",
                "description": "Optional: filter to specific symbol"
            }
        },
        "required": []
    }
}
```

**Returns:**
```json
{
    "period_days": 30,
    "total_trades": 12,
    "winning_trades": 7,
    "losing_trades": 5,
    "win_rate_pct": 58.3,
    "total_realized_pnl_usd": 45.20,
    "total_fees_usd": 3.80,
    "average_win_usd": 14.30,
    "average_loss_usd": -10.20,
    "profit_factor": 1.40,
    "best_trade": {
        "symbol": "SOL",
        "pnl_usd": 28.50,
        "pnl_pct": 12.3,
        "holding_hours": 72
    },
    "worst_trade": {
        "symbol": "ETH",
        "pnl_usd": -22.00,
        "pnl_pct": -8.9,
        "holding_hours": 6
    },
    "average_holding_hours": 48.5
}
```

---

### `write_memo`

Persist a reasoning note for future AI cycles.

```json
{
    "name": "write_memo",
    "description": "Write a persistent memory note that will be shown to you in all future trading cycles. Use this to document your strategy logic, observations, and lessons learned.",
    "parameters": {
        "type": "object",
        "properties": {
            "text": {
                "type": "string",
                "description": "The memo text. Be specific: what you decided, why, and what you expect to happen."
            }
        },
        "required": ["text"]
    }
}
```

---

### `read_memos`

Read all previously written memos.

```json
{
    "name": "read_memos",
    "description": "Read all your persistent memory notes from previous cycles. ALWAYS call this at the start of each cycle to remember your past reasoning.",
    "parameters": {
        "type": "object",
        "properties": {},
        "required": []
    }
}
```

---

## Context Tools (supplementary)

### `get_macro_context`

Get the latest global macro analysis (from MacroWorker).

```json
{
    "name": "get_macro_context",
    "description": "Get the AI-generated macro market analysis: cycle stage (bull/bear/accumulation), BTC/ETH dominance, total market cap, stablecoin flows.",
    "parameters": {
        "type": "object",
        "properties": {},
        "required": []
    }
}
```

### `get_news_sentiment`

Get AI-summarized news sentiment for a symbol.

```json
{
    "name": "get_news_sentiment",
    "description": "Get the latest AI-summarized news and sentiment analysis for a specific asset. Updated every 2 hours by the NewsWorker.",
    "parameters": {
        "type": "object",
        "properties": {
            "symbol": {
                "type": "string",
                "description": "Asset symbol, e.g. 'BTC'"
            }
        },
        "required": ["symbol"]
    }
}
```

### `get_correlation_matrix`

Get pairwise price correlations between tracked assets.

```json
{
    "name": "get_correlation_matrix",
    "description": "Get 30-day price correlation matrix for specified assets. Use this to avoid over-concentrating in correlated assets.",
    "parameters": {
        "type": "object",
        "properties": {
            "symbols": {
                "type": "array",
                "items": { "type": "string" }
            }
        },
        "required": ["symbols"]
    }
}
```

---

## Tool Execution Flow

Each AI cycle follows this sequence:

```
1. AI calls: read_memos()           → Get past reasoning
2. AI calls: get_macro_context()    → Get market cycle stage
3. AI calls: get_portfolio_state()  → Get current positions
4. AI calls: read_market_snapshot() → Get current prices
5. AI calls: list_active_triggers() → See pending orders
6. AI calls: review_trade_history() → Learn from past
7. AI reasons and decides...
8. AI calls: cancel_trigger(id)     → Remove outdated triggers
9. AI calls: set_trigger(params)    → Place new orders
10. AI calls: write_memo(text)      → Document reasoning
```

---

## AI Output Format

The AI must output a JSON array of tool calls:

```json
{
    "tool_calls": [
        {
            "tool": "cancel_trigger",
            "parameters": {
                "trigger_id": 42,
                "reason": "BTC has recovered above 62k; the 58k buy trigger is no longer relevant"
            }
        },
        {
            "tool": "set_trigger",
            "parameters": {
                "symbol": "ETH",
                "condition": "PriceBelow",
                "target_price": 2500.00,
                "action": "Buy",
                "amount_usd": 100.00,
                "expires_in_hours": 72,
                "reasoning": "ETH is approaching strong support at $2500 which held twice in the last 30 days. RSI on daily is oversold. Macro context shows accumulation phase. Buying 10% of portfolio for a swing trade to $2800."
            }
        },
        {
            "tool": "write_memo",
            "parameters": {
                "text": "Aug 5: Cancelled BTC buy at 58k (market recovered). Set ETH buy at 2500 support. Expecting 12% swing to 2800 within 1-2 weeks. Will set trailing stop after entry."
            }
        }
    ]
}
```

---

## Error Handling

If a tool call fails (e.g., `set_trigger` rejected by pre-check):

```json
{
    "tool": "set_trigger",
    "status": "error",
    "error": "Amount $150 exceeds 10% max position size of available balance ($650). Max allowed: $65.",
    "suggestion": "Reduce amount_usd to ≤ 65.00"
}
```

The AI sees this error and can retry with adjusted parameters in the same cycle.
