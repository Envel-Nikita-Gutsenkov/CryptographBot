# AI Request Payloads & Prompt Contract

This document describes exactly what Cryptograph sends to language models: the
data blocks, the prompt templates, the wire format per provider, and the parts
that are deliberately **not** sent. It is the reference for anyone debugging
prompts or adding a provider.

> The bot is **text-only**. No chart, image or other multimodal content is ever
> attached to a model request — PNG charts are rendered for Telegram only. All
> analysis is grounded in a pre-computed text snapshot.

---

## 1. Providers and routing

Requests go through `call_any_model` / `call_any_model_with_search`
(`src/crypto/ai/providers.rs`). The model id is resolved against the registry
(`models.json` + `models.openrouter.json`), which maps every alias to a concrete
`(provider, model id)` pair. Ten provider adapters exist:

| Provider | Wire format |
|---|---|
| Gemini | `POST https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key=…` |
| Groq, Mistral, Nvidia, OpenRouter, Cerebras, Cohere, Hugging Face, DeepSeek | OpenAI-compatible or provider-specific REST (see `providers.rs`) |
| Cloudflare | Cloudflare Workers AI REST |

If one provider/model fails, the call falls back to the next registered provider
for the same alias. Quota exhaustion (`429`, Cloudflare neurons, Groq TPD, Gemini
daily) parks the entire provider until its reset time instead of retrying.

The Gemini request body is minimal — a single text part, no `inlineData`:

```json
{
  "contents": [
    { "parts": [ { "text": "<full prompt>" } ] }
  ]
}
```

---

## 2. Prompt templates

All prompts live in `src/crypto/ai/prompts.rs`. The main ones:

| Function | Used by |
|---|---|
| `build_full_prompt` | `/ai SYMBOL` single-asset deep dive |
| `build_multi_prompt` | `/ai` multi-asset comparison |
| `build_watcher_prompt` | AI market watcher (JSON contract) |
| `build_ask_prompt` | `/ask` conversational answers |
| `build_moe_critic_prompt` | Critic pass over generator drafts |
| `build_proactive_*` | Goal/memo scans (draft, merge, critique, reflect) |

### `/ai SYMBOL` — market snapshot block

The prompt is assembled in `src/bot/handlers/ai.rs`. The injected data is:

```text
Coin: {name} ({symbol})
Price: {price} USD
Rank: #{rank}

Price Changes:
1h: {x}%  24h: {x}%  7d: {x}%
30d: {x}  90d: {x}  180d: {x}  365d: {x}

Market Stats:
24h Volume: {usd}
Market Cap: {usd}
Vol/MCap Ratio: {ratio}
Circulating Supply: {supply} {symbol}

Technical Indicators:
Momentum (7d): {Strong Uptrend … Strong Downtrend}
RSI (14d approx): {value | N/A}

Market Context:
Fear & Greed Index: {value} ({label})
{global market block: total cap, volume, BTC/ETH dominance}
```

Historical changes (`30/90/180/365d`) come from the local `price_history`
store; intraday changes and market stats come from the consensus quote.
Missing values render as `N/A` — the model is told to reason only from what is
present.

### News block

`fetch_news_for_symbol(symbol, name, 15)` returns up to **15** recent articles
relevant to the asset. Source order:

1. CryptoCompare public news API (keyless). Empty/error parks it for a cooldown.
2. Merged RSS feed (14 feeds behind a shared cache — see
   [system_brain_guide.md](system_brain_guide.md)).
3. Symbol filter, HTML strip, truncation, URL/title dedup.

### Portfolio block

When the user holds the asset (or on `/advice`), a portfolio section is injected:
allocation percentage, average entry, unrealised P&L and the user's memos/goals.
`/ask` additionally injects a `market_memory` block: 30/90-day OHLCV with RSI,
SMA, support/resistance and volatility, a 30-day text trajectory, and the
trained-strategy winners for each held symbol.

---

## 3. Output contract

Every template ends with the same rules:

- **Language mirroring** — answer in the language of the user's request (the
  bot's `preferred_language` setting also seeds this).
- **No `$` symbol** — it breaks Telegram's parser; all amounts use `USD`.
- **Markdown** — `**bold**`, `*italic*`, `#` headings, `-`/`1.` lists, and
  Markdown tables are allowed. Raw unicode bullets are forbidden.
- **Sectioned response** — for `/ai`: Technical Analysis, Sentiment & News,
  Forecast (direction + probability + timeframe), Outlook, Portfolio Strategy,
  Rating.
- Long-term spot holding assumptions only: no leverage, no shorts, no stop-loss
  advice.

The critic pass receives the same numeric snapshot and is instructed to correct
or reject any figure that does not match it before the answer is returned.

---

## 4. What is intentionally absent

- **No images / charts / PDFs** — text only.
- **No raw RSS dumps** — only compact, fresh (≤24 h) summaries reach a prompt.
- **No model-side arithmetic** — the critic forbids the model from computing
  percentages it can read from the snapshot.
- **No calendar-seasonality overreach** — deterministic models report sample
  size, and the AI is told to ignore statistically thin signals.
