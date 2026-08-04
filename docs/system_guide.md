# Cryptograph Complete System Guide

This guide describes the core architecture, data pipelines, database models, and algorithm workflows of Cryptograph. It is designed to help engineers understand exactly how data flows through the application and how various modules interact.

> [!TIP]
> For a detailed overview of AI model pools, request payloads, and dynamic promotion/routing logic, read the **[AI Pipeline & Model Orchestration Guide](docs/ai_pipeline_guide.md)**.

---

## 🏗️ 1. High-Level System Architecture

Cryptograph is a zero-trust, memory-first cryptocurrency portfolio tracker Telegram bot built in Rust. It utilizes Teloxide for event-driven bot commands, keeps active user portfolios in RAM, and leverages SQLite for non-sensitive public metadata caching.

### Architectural Diagram

```mermaid
graph TD
    User([Telegram User]) <-->|HTTPS/MTProto| Telegram[Telegram Bot API]
    Telegram <-->|Long Polling| Bot[Bot Module / Teloxide Handler]
    
    subgraph Storage Layer
        Bot <-->|Read/Write| RAMStore[In-Memory Decrypted State]
        RAMStore <-->|Backup / Load| SqliteDB[(SQLite: users table)]
        RAMStore -.->|AES-256-GCM Backup file| User
    end

    subgraph Price & Market Engine
        Bot <-->|Consensus Rate / Watchdog| CryptoClient[Crypto Client]
        CryptoClient <-->|Consensus API Queries| PriceProviders[CMC / Binance / Coinbase / Coingecko]
        CryptoClient <-->|FX Conversion| NBRB[NBRB Bank API - Daily BYN]
        CryptoClient -->|Cache Quote Metadata| SqliteDB
    end

    subgraph News Aggregation Engine
        NewsWorker[Background News Worker] -->|Fetch News| CryptoCompare[CryptoCompare News API]
        NewsWorker -->|Fallback Parallel Fetch| RssProvider[RSS Provider]
        RssProvider -->|Concurrent GET| RSSFeeds[CoinTelegraph / CoinDesk / Decrypt / Blockworks]
        NewsWorker -->|Deduplicate & Summarize| MoEPipeline[AI MoE Pipeline]
        MoEPipeline -->|Cache Summary| SqliteDB
    end

    subgraph AI Processing Pool
        Bot & NewsWorker <-->|Prompt Dispatch| MoEPipeline
        MoEPipeline <-->|Primary Pool: Groq / OpenRouter / Nvidia| CheapModels[Llama 3 / Qwen 2.5]
        MoEPipeline <-->|Last Resort Fallback| Gemini[Gemini 2.5 Flash]
    end
```

---

## 📰 2. News Aggregation Engine

The News Aggregation Engine runs dynamically and concurrently to feed the AI pipelines with high-quality market context while bypassing API limits and outages.

### Step-by-Step Flow

```mermaid
sequenceDiagram
    autonumber
    participant Worker as Background News Worker
    participant DB as SQLite Database
    participant API as CryptoCompare API
    participant RSS as RSS Provider (rss.rs)
    participant AI as AI pipeline.rs

    Worker->>DB: Check if summary for symbol exists and is fresh
    alt Cache Hits
        DB-->>Worker: Return Cached News Summary
    else Cache Miss / Invalidation
        Worker->>API: Query public News API
        alt API Success
            API-->>Worker: Return latest 15 articles
        else API Failure / Rate Limit (429)
            Worker->>RSS: Request RSS Fallback
            Note over RSS: Spawns 4 concurrent tokio tasks
            par CoinTelegraph
                RSS->>RSS: Fetch CoinTelegraph RSS
            and CoinDesk
                RSS->>RSS: Fetch CoinDesk RSS
            and Decrypt
                RSS->>RSS: Fetch Decrypt RSS
            and Blockworks
                RSS->>RSS: Fetch Blockworks RSS
            end
            RSS-->>Worker: Return combined lists (up to 40 articles)
        end
        Worker->>Worker: Deduplicate articles by normalized Title
        Worker->>Worker: Strip HTML tags & truncate bodies to 800 chars
        Worker->>AI: Send news corpus + market data
        AI->>AI: Try Qwen/Llama models (exclude Gemini)
        alt Primary Models Outage
            AI->>AI: Fallback to Gemini 2.5 Flash (Last resort)
        end
        AI-->>Worker: Return dense, cohesive summary
        Worker->>DB: Write summary into news_summaries (Update SQLite)
    end
```

---

## 💾 3. Database & Storage Architecture

Cryptograph divides data into two sections based on trust and sensitivity:
1. **Sensitive Portfolio Data**: Saved in-memory during execution, backed up via AES-256-GCM encryption.
2. **Public Market Metadata & Cache**: SQLite cache database (`cryptograph_cache.db` or `crypto.db`).

### Schema Definitions

The SQLite database contains the following tables:

#### 1. `users`
Stores user-encrypted binary portfolios. If the server restarts, this table initializes the RAM store.
- `id` (INTEGER PRIMARY KEY): Telegram User ID.
- `portfolio_data` (BLOB NOT NULL): Serialized JSON encrypted using AES-256-GCM (random 96-bit nonce, key derived from `MASTER_KEY`).
- `last_updated` (DATETIME): Timestamp of last backup.

#### 2. `news_summaries`
Caches the synthesized multi-source news digests.
- `symbol` (TEXT PRIMARY KEY): Cryptocurrency symbol in uppercase (e.g. `BTC`, `ETH`, `GENERAL`).
- `summary` (TEXT NOT NULL): AI-generated Markdown/Plaintext bulleted summary.
- `updated_at` (DATETIME): Timestamp of last worker cycle.

#### 3. `quote_meta_cache`
Caches non-real-time token metadata (rank, supply, historical percentage changes) to prevent API rate-limiting.
- `symbol` (TEXT PRIMARY KEY)
- `rank` (INTEGER NOT NULL)
- `circulating_supply` (REAL NOT NULL)
- `change_1h` / `change_24h` / `change_7d` / `change_30d` / `change_90d` (REAL)
- `changes_updated_at` (DATETIME)
- `updated_at` (DATETIME)

#### 4. `price_history` & `events`
Logs historical price points (for volatility metrics) and user-triggered audit logs. Kept for a maximum of 30 days (auto-cleaned via `CleanupOldEvents`).

---

## ⚙️ 4. Core Algorithmic Workflows

### 📊 Algorithmic Multi-Provider Price Consensus

To avoid anomalies or single-point-of-failure issues with free API providers:

```mermaid
flowchart TD
    Start([Request Quote for Token]) --> Query[Query CMC, Binance, Coinbase, Coingecko in parallel]
    Query --> FetchOK{At least 2 rates returned?}
    FetchOK -- Yes --> Outliers[Apply Outlier Rejection: Calculate Median]
    FetchOK -- No --> UseSingle[Use the single available provider rate]
    Outliers --> CheckBYN{Is local currency Belarus Ruble BYN?}
    CheckBYN -- Yes --> FetchNBRB[Fetch official Belarus National Bank NBRB rate daily]
    FetchBYN -- No --> Output[Return Consensus Rate in USD]
    FetchNBRB --> Convert[Multiply USD rate by USD/BYN exchange rate]
    Convert --> Output
    UseSingle --> CheckBYN
```

---

### 🧠 Mixture of Experts (MoE) Validation Pipeline

For complex analytics commands (`/ai` or `/advice`), a dual-agent validation loop prevents hallucinations.

```mermaid
flowchart TD
    Start([Execute AI Prompt]) --> Generators[Retrieve Available AI Pool]
    Generators --> Spawn[Spawn 2 Parallel Generator Tasks: race_dynamic_generators]
    Spawn --> Drafts{Both drafts received?}
    Drafts -- Yes --> CriticPrompt[Build Critic Prompt: news_judge_prompt]
    Drafts -- No / Fallback --> SingleDraft[Use single successful draft or retry with next model]
    CriticPrompt --> Critic[Run Critic Model: evaluate differences, fact-check, select best summary]
    Critic --> Output([Clean final output and append model signature])
```

---

### 🔔 Alert Engine Watchdog & Anti-Spam Workflow

Alerts run on a 5-minute tick background loop.

```mermaid
flowchart TD
    Tick([5-Minute Timer Tick]) --> Fetch[Fetch live consensus quotes for all alert symbols]
    Fetch --> Loop[Loop through active user Alert Rules]
    Loop --> Condition{Has threshold condition been met?}
    Condition -- Yes --> CheckSpam{Has this condition met in the PREVIOUS 5-min check?}
    Condition -- No --> ClearState[Clear consecutive triggers flag]
    CheckSpam -- Yes --> Cooldown{Has the configured Cooldown period e.g. 12h elapsed?}
    CheckSpam -- No --> SaveState[Mark 1st consecutive trigger and do not notify]
    Cooldown -- Yes --> Dispatch[Dispatch Telegram Alert Push Notification]
    Cooldown -- No --> Silenced[Silence alert and log event]
    Dispatch --> UpdateCooldown[Update last_triggered timestamp]
```
