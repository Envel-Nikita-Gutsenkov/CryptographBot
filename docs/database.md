# Database & Data Models Architecture

Cryptograph takes a unique **memory-first, zero-trust** approach to database management. It combines a highly secure, encrypted in-memory state for sensitive user portfolio data, and a fast SQLite cache for public market data.

---

## 1. The Core Portfolio Store (`src/db/store.rs`)

The `Store` is the central brain of Cryptograph. It holds all user portfolios, alerts, purchases, and settings completely in RAM while the bot is running.

### Why In-Memory?
- **Zero-Trust Security**: No sensitive financial data (like how much Bitcoin a user holds, or what their hardware wallet addresses are) is ever written to disk in plain text.
- **Speed**: Evaluating hundreds of price alerts every minute across thousands of assets is instant because there are no disk I/O bottlenecks.

### Versioned JSON Backups
Because data lives in RAM, it must be persisted to survive restarts.
- Every time a user mutates their portfolio (e.g., adds a purchase, creates a wallet), the `Store` automatically serializes their portfolio into versioned JSON via `serde_json` (`src/db/migration.rs` — the `BackupEnvelope` schema tag).
- `/backup` exports the portfolio as `cryptograph_backup.json`; `/restore` re-imports it. Legacy `.cgrp` files are still accepted for backward compatibility, but the format is plain JSON — **not encrypted**, so treat backup files as sensitive.
- The same JSON is stored in the SQLite `users` table for crash recovery.

---

## 2. The Data Models (`src/models/`)

The internal state is composed of strict Rust structs. The modular design separates different domain concerns into their own files to prevent monolithic "spaghetti" code.

### `portfolio.rs`
The aggregate root. Contains:
- `user_id`: Links the portfolio to the Telegram user.
- `purchases`: Vector of all spot purchases.
- `wallets`: Registered wallets (hot, cold, exchange).
- `alerts`: Active price alerts.
- `transfers`: Immutable ledger of asset movements between wallets.
- `settings`: The user's specific settings (AI preferences, currency, etc.).

### `purchase.rs`
Records the acquisition of an asset. 
- **Immutability**: The `cost_usd` (cost basis) is considered the ground truth.
- **Audit Trails**: If a user corrects a mistake via the `/correct` command, the original data is preserved, and a `CorrectionEntry` is appended to the `corrections` vector. This ensures a perfect audit trail of all manual edits.

### `holding.rs`
A dynamic calculation. Holdings are not strictly saved as static balances but are derived in real-time from `Purchases`, `Transfers`, and blockchain synchronizations to ensure absolute accuracy.

### `wallet.rs`
Supports `Cold` (Hardware), `Hot` (Software), and `Exchange` (CEX) definitions.
- Includes fields for on-chain tracking (`address`, `network`).
- Stores a `last_balance` hashmap to compute deltas when the `BlockchainClient` runs its polling loops.

### `alert.rs`
Defines robust alerting conditions:
- `PriceAbove` / `PriceBelow` (e.g., BTC > 100,000)
- `DropPercent` / `RisePercent` (e.g., BTC drops more than 10% in 4 hours)
- Features a `cooldown` mechanism to prevent notification spam during high volatility.

---

## 3. The SQLite Cache (`src/db/sqlite.rs`)

While the primary portfolio is in-memory, Cryptograph uses a local SQLite database (`cryptograph_cache.db`) for storing **non-sensitive, public data**.

### What goes into SQLite?
- **AI News Summaries**: The 2-hour dual-agent news worker fetches raw news, synthesizes it into 3 bullet points, and caches it in SQLite. When the 6-hour Watcher runs, it simply reads the pre-validated text from SQLite, saving immense API costs and context limits.
- **Historical Snapshots**: Price snapshots used for calculating precise percentage drops over custom timeframes (e.g., exactly 4 hours ago).

### Why SQLite?
- It is lightweight, requires no separate server (unlike PostgreSQL or Redis), and is perfectly suited for ephemeral caching. 
- If the SQLite file is deleted, the bot simply rebuilds the cache from external APIs. No user data is lost.
