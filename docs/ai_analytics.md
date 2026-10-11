# AI Analytics & Mixture of Experts (MoE) Architecture

Cryptograph uses a highly advanced, multi-agent AI architecture to provide deep market intelligence, spot accumulation strategies, and automated monitoring.

## Mixture of Experts (MoE) & Dual-Agent Validation

To ensure the highest quality of analysis and entirely eliminate "hallucinations" or contradictory logic, Cryptograph does not rely on a single LLM pass. Instead, it utilizes a **Generator-Critic Pipeline**:

1. **Writer (Generator)**: The primary model drafts the initial analysis based on raw market data and news.
2. **Judge (Critic)**: A secondary model reviews the draft. It actively searches for factual errors, contradictions, mathematical mistakes, or overly emotional/vague claims.
3. **Evaluator (Finalizer)**: If the Judge finds critical issues, the pipeline either forces a rewrite or synthesizes the critique into a corrected final output.

## Context Enrichment (No "Blind" Models)

Models are fed a comprehensive, "terminal-like" snapshot of the market to ensure they understand long-term and short-term trends. The injected context includes:
- **Historical Price Changes**: Exact percentage changes over `1h`, `24h`, `7d`, `30d`, `90d`, `180d`, and `365d` (1 year).
- **Technical Indicators**: RSI (Relative Strength Index) and algorithmic momentum signals.
- **Global Context**: Fear & Greed Index, Total Market Cap, 24h Volume, and BTC/ETH Dominance.
- **Fundamental Context**: Real-time summaries of the last 15 news articles for the specific asset.
- **Personal Portfolio Context**: If the user holds the asset, the exact allocation percentage of their portfolio is injected so the AI can advise based on their specific risk profile.

---

## Interactive AI Features

### 1. `/ai <SYMBOL>` (Single Asset Deep Dive)
Generates a comprehensive analysis for a specific coin.
- **Technical Analysis**: Trends, chart patterns, support/resistance.
- **Sentiment & News**: News impact.
- **Forecast**: Direction (Bullish/Bearish/Neutral), probability %, and timeframe.
- **Portfolio Strategy**: Actionable insight based on the user's existing portfolio.

### 2. `/ai` (Multi-Asset Analyzer)
Analyzes a predefined watchlist of assets (configured via `/ai set BTC ETH SOL`). Compares them and picks the best asset for spot accumulation right now based on risk/reward.

### 3. `AI Portfolio Advice` (Inline Button)
Located in the `/portfolio` menu. This feature reads the user's **actual portfolio holdings**, fetches 30-day metrics, RSI, and market cap for every held asset, and generates a personalized, long-term spot accumulation strategy. It advises which assets are overweight, which are undervalued, and where to deploy new capital.

---

## Automated AI Monitoring

### Background AI Watcher
Runs automatically every **6 hours** in the background.
- Scans all user portfolio symbols, watchlist, and major assets.
- Feeds 7-day volatility (min/max prices) and recent news summaries into the MoE pipeline.
- Remembers previous analyses to prevent contradictory "360-degree flips" in opinion.
- Sends a Telegram push notification **only** if a major market shift is detected.

**Notification Cooldowns:**
| Severity | Condition | Min interval |
|----------|-----------|--------------|
| 🚨 Critical | Extreme move (>15%), flash crash | 12 hours |
| 📢 Important | Notable move (5-15%), trend reversal | 48 hours |
| _(none)_ | Normal activity | No notification |

### Background News Worker (Dual-Agent Pipeline)
Runs every **2 hours** to summarize market news into concise bullet points.
- **Dual-Source Aggregator**: Pulls from the CryptoCompare News API as the primary channel. If rate-limited or unavailable, it falls back to a parallelized RSS collector gathering feeds from CoinTelegraph, CoinDesk, Decrypt, and Blockworks. Articles are merged and deduplicated.
- **Agent 1 (FastWorker)**: Drafts the summary.
- **Agent 2 (Flagship Controller)**: Verifies the summary against the previously saved database state to reject hallucinations.
- **SQLite Caching**: Verified summaries are cached in SQLite so the main 6-hour Watcher does not need to re-read raw news, saving massive context window space.

---

## Provider Fallbacks & Pools

Cryptograph supports multiple AI providers: **Gemini, Groq, Cerebras, Mistral,
Nvidia, Cohere, Cloudflare, Hugging Face, OpenRouter (free tier)**, with a
paid **DeepSeek** model as a budget-capped last resort.
To bypass rate limits and outages, it uses a **Fallback Pool System**:
- If a primary model hits a `429 Too Many Requests` or `503 Service Unavailable`, the pipeline automatically falls back to alternative providers in the pool.
- **Model Segment Routing**: High-volume recurring tasks (like news digestion) prioritize cost-efficient models (e.g., Qwen/Llama) and exclude Gemini models from the primary list. Gemini (e.g., `gemini-2.5-flash`) is reserved strictly as the final fallback layer. This balances speed, limits, and cost effectively.
- **Interactive Deep Dives**: Multi-modal workflows (such as `/ai <SYMBOL>` which processes price charts) utilize visual-capable models (e.g. Gemini 2.5) to analyze support, resistance, and market structures directly.

