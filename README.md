<div align="center">
  <h1>🤖 Cryptograph</h1>
  <p><b>The AI-Powered Telegram Crypto Intelligence Bot.</b></p>
  <p><i>24/7 market monitoring. Knows your wallet. Answers questions. Warns you first.</i></p>

  <p>
    <a href="#features"><img src="https://img.shields.io/badge/Architecture-Rust-f74c00?style=for-the-badge&logo=rust" alt="Rust"></a>
    <a href="#features"><img src="https://img.shields.io/badge/AI-MoE%20%2B%20Critic-4b32c3?style=for-the-badge&logo=openai" alt="AI"></a>
    <a href="#license"><img src="https://img.shields.io/badge/License-AGPLv3-blue?style=for-the-badge" alt="License"></a>
  </p>
</div>

> [!WARNING]
> **Personal Pet Project — No Support, No Liability**
>
> This is a **personal hobby project** built for my own use. I am **not** maintaining it for anyone else and I take **no responsibility** for anything that happens if you run, modify, or rely on it. Use it entirely at your own risk.
>
> **Cryptograph is NOT financial advice.** It is an AI assistant: large language models **hallucinate**, can use **outdated or wrong data**, and **will make mistakes** in market analysis and recommendations. Never invest money you cannot afford to lose, never follow an AI suggestion blindly, and always do your own research. The bot cannot execute trades — but even its *advice* can be wrong.
>
> If you deploy it: you are responsible for your own keys, your own server, and your own money. Do not complain to me.

---

## 🛑 The Problem

There is no universal crypto assistant. Today you have to juggle:

1. **A portfolio tracker** — knows what you bought, but knows nothing about the market.
2. **A price app** — knows live prices, but nothing about *your* positions or news.
3. **A news feed** — drowns you in noise, no connection to your holdings.
4. **A generic AI chat** — smart, but **has no data**: it doesn't know your wallet, your average entry price, the real current prices, or today's news. So it answers from memory and **hallucinates**.

None of them combine all of it. None of them *proactively* tell you "hey, your target condition was met" — you have to keep asking.

## ⚡ The Cryptograph Solution

**Cryptograph is a single self-hosted Telegram bot that fuses everything into one AI brain:**

* **It knows your portfolio** — every purchase, wallet, average entry price, invested amount, P&L.
* **It knows the market** — live prices from **12 independent providers** with consensus (CoinMarketCap, CoinGecko, Binance, Bybit, Coinbase, CoinPaprika, DefiLlama, DexScreener, GeckoTerminal, and more), 1h/24h/7d/30d/90d/365d changes, ATH discounts, global market cap, BTC/ETH dominance, Fear & Greed.
* **It knows the news** — a dedicated worker continuously fetches and AI-summarizes news for your symbols.
* **It knows on-chain & derivatives** — BTC network health, funding rates, open interest, stablecoin flows, macro cycle context.
* **It monitors 24/7** and **decides by itself when to warn you** — not on a schedule, but when conditions genuinely matter.

Because it is fed *real, pre-computed data* ("on a platter"), Cryptograph **answers better than any generic AI**: the model doesn't have to guess prices or invent numbers — it reads them from the snapshot you and it can both verify.

### 🧠 How the AI Works

* **MoE double-checked pipeline:** every important answer is produced by **two independent models racing**, then reviewed and merged by a **fact-checking critic** that validates every number against the ground-truth market data. Hallucinated prices get corrected or rejected.
* **Web search:** if a question needs fresh information (wallets, regulations, latest news), the model can search the web — Tavily / Exa first, then a free local fallback.
* **Free-first:** runs entirely on **free API tiers** (Gemini, Groq, Cerebras, NVIDIA, Mistral, Cohere, Cloudflare, OpenRouter free). A paid **DeepSeek** model is available as a last resort, with a hard **monthly budget enforced in code** (default 3M tokens / 150 requests) so it can never be burned by accident. OpenRouter has a hard **daily 300-request budget**.
* **Self-improving model pool:** a lazy ranker (`models refresh|rank|stats|calibrate`) discovers free models across all providers, benchmarks **at most 3 per day** (30-day cooldown, fingerprint-change detection) with a non-OpenRouter judge, and assigns roles — so the pools self-tune without burning rate limits.
* **Self-healing:** model health is tracked from real traffic only; failing models get a 1-hour cooldown; workers restart automatically on panic with exponential backoff; schema self-migrates on every boot.

### 🎯 Goals & Memory (Proactive Mode)

* **`/goal`** — tell the bot what you want ("spend 400 USD on crypto this month", "alert me on top discounts so I don't raise my average entry", "alert me when I'm up x2"). It understands dates and expiry, watches the market, and **alerts you only when conditions actually fit** — with a 12h per-goal cooldown, quiet-hours support, and its own conservative double-check. When a goal is achieved or becomes stale it tells you **why**.
* **`/memo`** — standing notes the AI always remembers ("always invest 100 USD at a time", "BTC is on hold, watch alts").
* You manage everything with `/goal list`, `/goal done N`, `/goal del N`, `/memo list`, `/memo del N`.

---

## ✨ Why Cryptograph? (Key Capabilities)

### 1. 🔌 Runs on Free APIs Only
No paid SaaS required. Price data works through free tiers (CoinGecko demo, CoinPaprika, Binance public, Bybit public, Coinbase public, DefiLlama, GeckoTerminal…), AI works through free model tiers. The only optional paid component is DeepSeek, and it is budget-capped in code.

### 2. 🛡️ You Own Your Data
Fully self-hosted. Everything — portfolio, history, stats, goals, memos — lives in a **single local SQLite database** (WAL mode). No third party ever sees your positions. Backups are exported as portable, versioned **JSON** files (`cryptograph_backup.json`) that you can inspect and re-import at any time.

### 3. 💾 Survives Updates & Crashes
* Goals, memos and wallet data live in dedicated tables and are restored on boot — a redeploy **never** wipes your data.
* Only ephemeral caches (model health, stale metrics) are reset on restart.
* A watchdog guards the process; the alert engine, AI workers, and the proactive goal worker are all supervised and auto-restart.

### 4. 📈 High-Fidelity Charts
P&L breakdowns, portfolio allocation donuts, and historical price charts rendered as PNG — with **data validation** that filters provider glitches so you don't see fake x2 spikes that never happened.

### 5. 🧹 No Spam
Price alerts have cooldowns and quiet hours. The AI watcher and the goal worker decide *whether* to message you, and the goal worker is explicitly instructed to prefer silence over noise.

### 6. 🧭 Network Efficiency
Quotes are served from a local cache (180s TTL) with real-time refresh by the Binance WS stream; providers are fanned out only on cache misses — dozens of times fewer provider calls than naive per-request polling. Providers left without an API key are skipped instead of being called just to fail. Deterministic analytics (7-day min/max, volatility gates, milestone anchors) are computed from local SQLite history — no AI and no network calls where plain math is enough.

---

### 🧩 Feature Suite

**Market Intelligence**
* `/price BTC ETH` — live quotes with changes & ATH discounts
* `/top N` — top movers by 24h volume
* `/convert 100 USD BTC` — converter
* `/fng` — Fear & Greed Index

**Portfolio Management**
* `/add BTC 0.01 600 hot:binance` — record a purchase
* `/move`, `/adjust`, `/correct`, `/invested` — full cost-basis management
* `/portfolio`, `/pnl`, `/distribution` — text + PNG charts
* `/wallet add|list|refresh` — wallets with real on-chain balances (Helius/Ankr)

**AI Assistant**
* `/ask question` — ask about your portfolio + market + news, with memory of your memos & goals
* `/ai BTC` / `/ai set BTC ETH` — deep multi-asset analysis
* `/patch` — fix portfolio from plain text
* `/advice` — long-term investment intelligence
* `/news BTC` — AI news summaries
* `/model list` — pick your preferred model

**Proactive**
* `/goal ...`, `/memo ...` — the AI watches the market for your goals 24/7

**Alerts & Notifications**
* `/alert add BTC < 60000 cooldown:6h`, `/alert list`, `/alert del N`
* AI market watcher with severity levels (Critical/Important) and cooldowns

**Trading (CLI + Telegram — Win-Win Accumulator)**
* `trade allocate SOL 50` — isolated capital pool
* `trade rule BTC ceiling 65000` — operator limits ("don't buy above $65k")
* `trade tick SOL 75 --execute` — deterministic engine cycle
* `trade backtest --batch --symbols BTC,ETH,SOL,LINK --from 2020-01-01 --to 2026-09-01`
* `trade strategies` / `trade strategy apply Win-Win-Accumulator` / `trade tune`
* `/trade ...`, `/tradetask ...` — Telegram mirrors (sim mode by default)

**System & Privacy**
* `/settings`, `/stats` (model health, data-source reliability, web-search usage, DeepSeek budget)
* `/backup`, `/restore` — versioned JSON backup
* `/allow`, `/revoke`, `/acl` — access control

---

## 🛠️ Requirements

| Variable | Required | Purpose |
|---|---|---|
| `TELOXIDE_TOKEN` | ✅ | Your Telegram bot token |
| `ALLOWED_ADMIN_IDS` | ✅ | Comma-separated Telegram user IDs allowed to use the bot |
| `GEMINI_API_KEY` | ◻️ | Free AI tier (strongly recommended — main fallback model) |
| `GROQ_API_KEY`, `CEREBRAS_API_KEY`, `NVIDIA_API_KEY`, `MISTRAL_API_KEY`, `COHERE_API_KEY`, `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`, `HF_API_KEY` | ◻️ | Additional free AI tiers (the more, the more resilient) |
| `OPENROUTER_API_KEY` | ◻️ | Free-tier model discovery + ranking; hard daily 300-request budget |
| `DEEPSEEK_API_KEY` | ◻️ | Optional paid last-resort model (budget-capped: 3M tokens / 150 req per month) |
| `CMC_API_KEY`, `COINGECKO_API_KEY`, `COINCAP_API_KEY`, `CRYPTOCOMPARE_API_KEY` | ◻️ | Market data providers (free tiers are fine; a provider without a key is auto-skipped) |
| `HELIUS_API_KEY`, `ANKR_RPC_URL` | ◻️ | On-chain wallet balances |
| `TAVILY_API_KEY`, `EXA_API_KEY` | ◻️ | Web search for the AI |
| `SQLITE_DB` | ◻️ | DB path (default `crypto.db`, `/app/data/crypto.db` in Docker) |

**The bot works with just a Telegram token + Gemini free key.** Every other key adds reliability, data sources, or features — not a hard requirement. See `.env.example` for the full template.

* **Rust 1.75+** for local builds; **Docker 20.10+** recommended for production.
* **Server:** works on x86_64 and **ARM64** (the Docker image is built for ARM64).

---

## 🚀 Deployment

### Method 1: Local Development
```bash
cp .env.example .env      # then fill in your keys
cargo run
```

### Method 2: Production (ARM64 VPS, recommended)
The workflow is "build on your machine, deploy on the server" — no Rust toolchain needed on the server.

1. **On your dev machine (Windows/PowerShell):**
   ```powershell
   .\build_dist.ps1
   ```
   This packages the source + `models.json` into a `dist` folder.

2. **Copy `dist/` to the server** (SCP/SFTP/Rsync).

3. **On the server:**
   ```bash
   chmod +x run.sh
   ./run.sh
   ```
   `run.sh` builds the ARM64 Docker image, starts the container, and prints the SQLite path so you can verify data persistence.

### 🔄 Updating
Same workflow — repackage with `build_dist.ps1`, copy to the server, `./run.sh`. **Your data survives:** the SQLite volume is preserved, the schema migrates itself, and only ephemeral caches are reset. No manual database commands ever needed.

### 🧪 CLI Diagnostics
The bot ships with built-in CLI modes to see *exactly* what the AI does — and to
hard-test every subsystem 1:1 with the Telegram code paths, without touching
Telegram:
```bash
cargo run -- analyze BTC                 # deep AI analysis in the terminal
cargo run -- ask demo "your question"    # replay /ask with prompts, web searches, drafts, critic
cargo run -- goal demo                   # replay a proactive goal scan, stage by stage
cargo run -- scenario scenarios/euphoria.json "стоит ли докупать?" --fast  # deterministic what-if
cargo run -- search "новости по ETH"     # inspect only the web-search flow
cargo run -- models refresh|rank|stats   # lazy model ranking across providers
cargo run -- trade status                # trading module (sim by default)
cargo run -- wipe-knowledge              # reset caches, keep users/goals/memos/budgets
```

---

## 🔒 Security Notes

* Real API keys live **only** in `.env`, which is gitignored and never committed.
* Backups are portable versioned JSON files (not encrypted) — treat them as sensitive, anyone with the file can read your positions.
* Access is restricted to `ALLOWED_ADMIN_IDS`.
* The systemd unit ships with hardening (no privileges, private tmp, read-only root).

---

## 📚 Documentation

- [Installation & Deployment](docs/installation.md)
- [Features](docs/features.md)
- [Architecture](docs/architecture.md)
- [Database](docs/database.md)
- [Complete System Guide](docs/system_guide.md)
- [AI Pipeline & Model Orchestration](docs/ai_pipeline_guide.md)
- [System Brain & Scheduler](docs/system_brain_guide.md)
- [AI Analytics & MoE](docs/ai_analytics.md)
- [Backup & Restore](docs/backup_restore_guide.md)
- [Trading Module (Win-Win Accumulator)](docs/trading/README.md)

---

## 📄 License

**AGPL-3.0** — free to use, modify, and self-host. If you run it as a networked service for others, share your changes under the same license.
