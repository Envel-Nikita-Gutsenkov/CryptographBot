# Gemini AI Analysis Integration

This document outlines the details of the Gemini AI integration in the Cryptograph bot, specifying what data is sent to the model, how the prompt is structured, and how the visual chart and news analysis are handled.

---

## 1. Data Sent to Gemini AI

When a user requests an AI analysis for a cryptocurrency (e.g., **BTC**), the bot aggregates multiple data sources to provide Gemini with maximum context:

### A. Real-Time Market Data (Consensus Quotes)
The bot gathers consensus price metrics from the active providers (CoinMarketCap, CoinGecko, CoinCap):
- **Current Price** (USD)
- **1-hour Price Change** (%)
- **24-hour Price Change** (%)
- **7-day Price Change** (%)
- **24-hour Trading Volume** (USD)
- **Market Capitalization** (USD)
- **Market Cap Rank**

### B. Historical Chart (Multimodal Visual Input)
The bot generates a high-resolution PNG chart representing the price history (1D, 7D, 30D, or 90D) using the built-in dark-themed `ChartRenderer`. 
- This image is base64-encoded and transmitted as an `inlineData` part in the API request.
- Gemini uses its visual capability to inspect technical patterns, trend lines, support/resistance zones, and indicators.

### C. Public Market News
The bot queries the public keyless CryptoCompare News API:
- Retrieves the latest 15 global cryptocurrency news articles.
- If the CryptoCompare API is rate-limited or fails, it falls back to the parallel RSS news aggregator (CoinTelegraph, CoinDesk, Decrypt, Blockworks) to retrieve up to 40 deduplicated articles.
- Filters articles containing references to the requested asset symbol or name.
- Passes the top relevant news titles and summaries as context for Gemini.

---

## 2. English System Prompt

The following system prompt is sent to Gemini alongside the image and text inputs to guide its analysis and ensure it responds in the user's preferred language (Russian):

```text
You are an expert financial analyst, professional cryptocurrency trader, and technical analyst.
You are analyzing the cryptocurrency: {symbol}.

Here is the current real-time market data for {symbol}:
- Current Price: ${price_usd} USD
- 1h Change: {change_1h}%
- 24h Change: {change_24h}%
- 7d Change: {change_7d}%
- 24h Volume: ${volume_24h} USD
- Market Capitalization: ${market_cap} USD
- Market Cap Rank: #{rank}

Recent relevant news articles for {symbol}:
{news_summary}

Attached is the price chart for {symbol} showing recent price movement.
Analyze the chart patterns, trend lines, support and resistance zones, current price action, and recent news.

Provide a concise, data-driven, and actionable market analysis in English. Your response must include:
- **Technical Analysis**: Identify trends, chart patterns, key support/resistance levels.
- **Sentiment & News**: Assess recent news and market fear/greed index impact.
- **Outlook**: Short-to-medium term targets and key risk levels.
- **Long-Term Strategy**: State clearly whether it is better to BUY NOW for the long-term or WAIT for a better entry point.
- **Investment Rating**: Provide a concrete overall score (e.g., 7/10) or rating (e.g., Bullish, Neutral, Bearish) with a brief justification.

Format using clean Markdown for Telegram (bold, lists). Keep it concise, direct, and free of fluff.
```

---

## 3. API Details & Integration Flow

### Endpoint
The integration communicates directly with the Google AI Gemini API using `reqwest`:
`POST https://generativelanguage.googleapis.com/v1/models/gemini-2.5-flash:generateContent?key={GEMINI_API_KEY}`

### Request Schema
```json
{
  "contents": [
    {
      "parts": [
        {
          "text": "<System Prompt + Market Data + News Summary>"
        },
        {
          "inlineData": {
            "mimeType": "image/png",
            "data": "<Base64 encoded PNG chart>"
          }
        }
      ]
    }
  ]
}
```
