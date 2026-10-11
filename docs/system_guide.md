# Cryptograph Complete System Guide

This guide describes the core architecture, data pipelines, database models, and algorithm workflows of Cryptograph. It is designed to help engineers understand exactly how data flows through the application and how various modules interact.

> [!TIP]
> For a detailed overview of AI model pools, request payloads, and dynamic promotion/routing logic, read the **[AI Pipeline & Model Orchestration Guide](ai_pipeline_guide.md)**.

---

## 🏗️ 1. High-Level System Architecture

Cryptograph is a zero-trust, memory-first cryptocurrency portfolio tracker Telegram bot built in Rust. It utilizes Teloxide for event-driven bot commands, keeps hot portfolio state in RAM, and persists everything — users, goals, memos, caches, budgets, model telemetry — in a single SQLite database (`crypto.db`, WAL mode) that self-migrates on every boot.

### Architectural Diagram

```mermaid
graph TD
    User([Telegram User]) <-->|HTTPS/MTProto| Telegram[Telegram Bot API]
    Telegram <-->|Long Polling| Bot[Bot Module / Teloxide Handler]
    
    subgraph Storage Layer
        Bot <-->|Read/Write| RAMStore[In-Memory Portfolio Store]
        RAMStore <-->|Versioned JSON Backup| SqliteDB[(SQLite: crypto.db)]
        SqliteDB <-->|Restore on boot| RAMStore
    end

    subgraph Price & Market Engine
        Bot <-->|Consensus Rate / Watchdog| CryptoClient[Crypto Client]
        CryptoClient <-->|Consensus API Queries| PriceProviders[CMC / Binance / Coinbase / Coingecko / CoinCap ...]
        CryptoClient <-->|FX Conversion| NBRB[NBRB Bank API - Daily BYN]
        CryptoClient -->|Cache Quote Metadata| SqliteDB
    end

    subgraph News Aggregation Engine
        NewsWorker[Background News Worker] -->|Fetch News| RssProvider[RSS Provider]
        RssProvider -->|Concurrent GET| RSSFeeds[CoinTelegraph / CoinDesk / Decrypt / Blockworks / The Block / Bitcoin Magazine]
        NewsWorker -->|Deduplicate & Summarize| MoEPipeline[AI MoE Pipeline]
        MoEPipeline -->|Cache Summary| SqliteDB
    end

    subgraph AI Processing Pool
        Bot & NewsWorker <-->|Prompt Dispatch| MoEPipeline
        MoEPipeline <-->|Provider Pool| Providers[Gemini / Groq / Cerebras / Mistral / Nvidia / Cohere / Cloudflare / OpenRouter / DeepSeek]
        MoEPipeline <-->|Budgets| Budgets[DeepSeek monthly / OpenRouter daily 300]
        Ranker[Model Ranker / daily] -->|Registry + Benchmarks| SqliteDB
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

Cryptograph divides data into two layers based on trust and sensitivity:
1. **User data (portfolios, goals, memos)**: persisted in SQLite (`users` as
   versioned JSON, `user_goals` / `user_memos` as dedicated tables) and loaded
   into RAM on boot. Backups are portable plain-JSON files — **not encrypted**.
2. **Public market metadata & caches**: SQLite tables (`quote_meta_cache`,
   `price_history`, `news_summaries`, `macro_cache`, `price_milestones`,
   `model_health`) rebuilt automatically from external APIs if deleted.

### Schema Definitions

The SQLite database contains the following tables:

#### 1. `users`
Stores the serialized portfolio JSON per Telegram user. On restart this table
re-initializes the RAM store.
- `id` (INTEGER PRIMARY KEY): Telegram User ID.
- `portfolio_data` (BLOB NOT NULL): Serialized JSON (`BackupEnvelope`).
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
Logs historical price points (for volatility metrics and the AI trajectory
memory) and user-triggered audit logs. Kept for a maximum of 30 days
(auto-cleaned via `CleanupOldEvents`).

#### 5. `user_memos` / `user_goals` / `proactive_cache`
Standing AI memory (hard rules), user goals with progress + verdicts, and the
last proactive scan snapshot. These survive cache wipes and `users` rebuilds.

#### 6. Budget tables — `monthly_usage`, `paid_usage`, `daily_usage`
Token/request counters for the DeepSeek monthly cap and the OpenRouter daily
300-request cap, mirrored in RAM by the budget singletons.

#### 7. Model ranking tables — `model_registry`, `model_stats`
Discovered models (with fingerprints and test state) and per-model daily
calls/errors/latency/quality. `model_stats.bench_calls` counts only
benchmark calls, so production traffic cannot dilute judge quality scores.

#### 8. Trading tables (`trading_sim.db` / `trading_live.db`)
`operator_constraints`, `strategy_allocations`, `trade_lots`, `agent_debates`,
`trade_executions`, `backtest_runs`, `strategy_lessons`, `engine_configs`,
`strategy_profiles` — see [Database & Data Models](database.md) §4.

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
