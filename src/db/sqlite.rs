use rusqlite::{params, Connection};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub enum SqliteMessage {
    SaveUser {
        user_id: u64,
        data: Vec<u8>,
    },
    #[allow(dead_code)]
    LogEvent {
        user_id: u64,
        event_type: String,
        data: String,
    },
    LogPrice {
        symbol: String,
        price: f64,
        open: Option<f64>,
        high: Option<f64>,
        low: Option<f64>,
    },
    SaveNewsSummary {
        symbol: String,
        summary: String,
    },
    SaveMacroSummary {
        symbol: String,
        summary: String,
    },
    SaveQuoteMeta {
        symbol: String,
        rank: u32,
        circulating_supply: f64,
        change_1h: Option<f64>,
        change_24h: Option<f64>,
        change_7d: Option<f64>,
        change_30d: Option<f64>,
        change_90d: Option<f64>,
    },
    SavePriceMilestone {
        symbol: String,
        milestone: String,
        price: f64,
        recorded_at: String,
    },
    FlushDailyMetrics {
        metrics: Vec<crate::models::MetricDelta>,
    },
    UpdateModelHealth {
        provider_id: String,
        status: String,
        error_msg: Option<String>,
    },
    /// Full JSON list of the user's memos (dedicated table, survives users-table rebuilds).
    SaveUserMemos {
        user_id: u64,
        data: String,
    },
    /// Full JSON list of the user's goals (dedicated table, survives users-table rebuilds).
    SaveUserGoals {
        user_id: u64,
        data: String,
    },
    /// Full JSON list of the user's /ask Q/A pairs (multi-turn dialogue memory).
    SaveAskHistory {
        user_id: u64,
        data: String,
    },
    /// Last proactive scan context per user (continuity for the goal-check AI).
    SaveProactiveCache {
        user_id: u64,
        snapshot: String,
        analysis: String,
    },
    CleanupOldEvents,
}

#[derive(Clone)]
pub struct SqliteStore {
    tx: mpsc::Sender<SqliteMessage>,
    db_path: String,
}

fn open_and_init_db(db_path: &str) -> anyhow::Result<Connection> {
    let mut conn = Connection::open(db_path)?;

    // Check integrity of the database file
    let mut is_ok = false;
    if let Ok(mut stmt) = conn.prepare("PRAGMA integrity_check;") {
        if let Ok(mut rows) = stmt.query([]) {
            if let Ok(Some(row)) = rows.next() {
                let res: String = row.get(0).unwrap_or_default();
                if res == "ok" {
                    is_ok = true;
                }
            }
        }
    }

    if !is_ok {
        return Err(anyhow::anyhow!("PRAGMA integrity_check failed"));
    }

    init_db(&mut conn)?;
    Ok(conn)
}

impl SqliteStore {
    fn normalize_cached_text(text: &str) -> String {
        crate::format::plain_for_cache(text)
    }
    pub fn new(db_path: &str) -> anyhow::Result<Self> {
        let (tx, mut rx) = mpsc::channel::<SqliteMessage>(1000);
        let db_path = db_path.to_string();
        let db_path_clone = db_path.clone();

        // Spawn a blocking thread for SQLite operations
        tokio::task::spawn_blocking(move || {
            let mut conn = match open_and_init_db(&db_path_clone) {
                Ok(c) => c,
                Err(e) => {
                    warn!("Database file at {} is invalid or corrupted: {}. Attempting auto-recovery...", db_path_clone, e);
                    let backup_path = format!(
                        "{}.corrupt.{}",
                        db_path_clone,
                        chrono::Utc::now().timestamp_millis()
                    );
                    let _ = std::fs::rename(&db_path_clone, &backup_path);

                    match open_and_init_db(&db_path_clone) {
                        Ok(c) => {
                            info!("Auto-recovery successful. Re-created fresh database at {}. Corrupt file moved to {}", db_path_clone, backup_path);
                            c
                        }
                        Err(err2) => {
                            error!("Auto-recovery failed to open new database: {}. SQLite persistence is DISABLED.", err2);
                            return;
                        }
                    }
                }
            };

            info!(
                "SQLite database initialized successfully at {}",
                db_path_clone
            );

            // Fresh model health every boot: stale DEAD/COOLDOWN marks from old
            // versions must never survive a deploy. Statuses repopulate from real
            // traffic (success → OK, failure → 1h COOLDOWN).
            if let Err(e) = conn.execute("DELETE FROM model_health;", []) {
                warn!("Failed to clear stale model health marks: {}", e);
            }

            while let Some(msg) = rx.blocking_recv() {
                match msg {
                    SqliteMessage::SaveUser { user_id, data } => {
                        let res = conn.execute(
                            "INSERT INTO users (id, portfolio_data) VALUES (?1, ?2)
                             ON CONFLICT(id) DO UPDATE SET portfolio_data = ?2, last_updated = CURRENT_TIMESTAMP",
                            params![user_id, data],
                        );
                        if let Err(e) = res {
                            warn!("Failed to save user {} to SQLite: {}", user_id, e);
                        }
                    }
                    SqliteMessage::LogEvent {
                        user_id,
                        event_type,
                        data,
                    } => {
                        let res = conn.execute(
                            "INSERT INTO events (user_id, event_type, data) VALUES (?1, ?2, ?3)",
                            params![user_id, event_type, data],
                        );
                        if let Err(e) = res {
                            warn!("Failed to log event {} to SQLite: {}", event_type, e);
                        }
                    }
                    SqliteMessage::LogPrice {
                        symbol,
                        price,
                        open,
                        high,
                        low,
                    } => {
                        let res = conn.execute(
                            "INSERT INTO price_history (symbol, price, open, high, low) VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![symbol, price, open, high, low],
                        );
                        if let Err(e) = res {
                            warn!("Failed to log price {} to SQLite: {}", symbol, e);
                        }
                    }
                    SqliteMessage::SaveNewsSummary { symbol, summary } => {
                        let res = conn.execute(
                            "INSERT INTO news_summaries (symbol, summary) VALUES (?1, ?2)
                             ON CONFLICT(symbol) DO UPDATE SET summary=excluded.summary, updated_at=CURRENT_TIMESTAMP",
                            params![symbol, summary],
                        );
                        if let Err(e) = res {
                            warn!("Failed to save news summary {} to SQLite: {}", symbol, e);
                        }
                    }
                    SqliteMessage::SaveMacroSummary { symbol, summary } => {
                        let res = conn.execute(
                            "INSERT INTO macro_cache (symbol, summary) VALUES (?1, ?2)
                             ON CONFLICT(symbol) DO UPDATE SET summary=excluded.summary, updated_at=CURRENT_TIMESTAMP",
                            params![symbol, summary],
                        );
                        if let Err(e) = res {
                            warn!("Failed to save macro summary {} to SQLite: {}", symbol, e);
                        }
                    }
                    SqliteMessage::SaveQuoteMeta {
                        symbol,
                        rank,
                        circulating_supply,
                        change_1h,
                        change_24h,
                        change_7d,
                        change_30d,
                        change_90d,
                    } => {
                        // We update `changes_updated_at` only if at least one change is SOME, otherwise we keep old values (if any) or null.
                        // However, to keep it simple, we just update the fields if they are Some, or overwrite them.
                        // Actually, since we only save when we have valid data, we can just overwrite.

                        let changes_updated_at = if change_1h.is_some()
                            || change_24h.is_some()
                            || change_7d.is_some()
                            || change_30d.is_some()
                            || change_90d.is_some()
                        {
                            Some(chrono::Utc::now().to_rfc3339())
                        } else {
                            None
                        };

                        let res = conn.execute(
                            "INSERT INTO quote_meta_cache (symbol, rank, circulating_supply, change_1h, change_24h, change_7d, change_30d, change_90d, changes_updated_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                             ON CONFLICT(symbol) DO UPDATE SET
                                rank=excluded.rank, circulating_supply=excluded.circulating_supply,
                                change_1h=COALESCE(excluded.change_1h, change_1h),
                                change_24h=COALESCE(excluded.change_24h, change_24h),
                                change_7d=COALESCE(excluded.change_7d, change_7d),
                                change_30d=COALESCE(excluded.change_30d, change_30d),
                                change_90d=COALESCE(excluded.change_90d, change_90d),
                                changes_updated_at=COALESCE(excluded.changes_updated_at, changes_updated_at),
                                updated_at=CURRENT_TIMESTAMP",
                            params![symbol, rank, circulating_supply, change_1h, change_24h, change_7d, change_30d, change_90d, changes_updated_at],
                        );
                        if let Err(e) = res {
                            warn!("Failed to save quote meta {} to SQLite: {}", symbol, e);
                        }
                    }
                    SqliteMessage::SavePriceMilestone {
                        symbol,
                        milestone,
                        price,
                        recorded_at,
                    } => {
                        let res = conn.execute(
                            "INSERT INTO price_milestones (symbol, milestone, price, recorded_at)
                             VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT(symbol, milestone) DO UPDATE SET
                                price=excluded.price,
                                recorded_at=excluded.recorded_at,
                                updated_at=CURRENT_TIMESTAMP",
                            params![symbol, milestone, price, recorded_at],
                        );
                        if let Err(e) = res {
                            warn!(
                                "Failed to save price milestone {}/{}: {}",
                                symbol, milestone, e
                            );
                        }
                    }
                    SqliteMessage::FlushDailyMetrics { metrics } => {
                        let tx = match conn.transaction() {
                            Ok(t) => t,
                            Err(e) => {
                                warn!("Failed to start transaction for daily metrics: {}", e);
                                continue;
                            }
                        };
                        let date_str = chrono::Utc::now().format("%Y-%m-%d").to_string();
                        for m in metrics {
                            let _ = tx.execute(
                                "INSERT INTO daily_metrics (category, item, date, ok_count, err_count) VALUES (?1, ?2, ?3, ?4, ?5)
                                 ON CONFLICT(category, item, date) DO UPDATE SET
                                 ok_count = ok_count + ?4,
                                 err_count = err_count + ?5",
                                params![m.category, m.item, date_str, m.ok, m.err],
                            );
                        }
                        if let Err(e) = tx.commit() {
                            warn!("Failed to commit daily metrics flush: {}", e);
                        }
                    }
                    SqliteMessage::CleanupOldEvents => {
                        let res = conn.execute(
                            "DELETE FROM events WHERE created_at < datetime('now', '-7 days')",
                            [],
                        );
                        if let Err(e) = res {
                            warn!("Failed to cleanup old events: {}", e);
                        }
                        let _ = conn.execute("DELETE FROM price_history WHERE timestamp < datetime('now', '-90 days')", []);
                        let _ = conn.execute(
                            "DELETE FROM daily_metrics WHERE date < date('now', '-31 days')",
                            [],
                        );
                        let _ = conn.execute(
                            "DELETE FROM paid_usage WHERE date < date('now', '-95 days')",
                            [],
                        );

                        // We do NOT run VACUUM here by default to avoid blocking the DB for too long
                        // and consuming extra I/O. SQLite will reuse the freed pages automatically.
                    }
                    SqliteMessage::UpdateModelHealth {
                        provider_id,
                        status,
                        error_msg,
                    } => {
                        let res = conn.execute(
                            "INSERT INTO model_health (provider_id, status, error_msg, last_checked)
                             VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)
                             ON CONFLICT(provider_id) DO UPDATE SET
                                status=excluded.status,
                                error_msg=excluded.error_msg,
                                last_checked=CURRENT_TIMESTAMP",
                            params![provider_id, status, error_msg.unwrap_or_default()],
                        );
                        if let Err(e) = res {
                            warn!("Failed to update model health for {}: {}", provider_id, e);
                        }
                    }
                    SqliteMessage::SaveUserMemos { user_id, data } => {
                        let res = conn.execute(
                            "INSERT INTO user_memos (user_id, data) VALUES (?1, ?2)
                             ON CONFLICT(user_id) DO UPDATE SET data=excluded.data, updated_at=CURRENT_TIMESTAMP",
                            params![user_id, data],
                        );
                        if let Err(e) = res {
                            warn!("Failed to save memos for user {}: {}", user_id, e);
                        }
                    }
                    SqliteMessage::SaveUserGoals { user_id, data } => {
                        let res = conn.execute(
                            "INSERT INTO user_goals (user_id, data) VALUES (?1, ?2)
                             ON CONFLICT(user_id) DO UPDATE SET data=excluded.data, updated_at=CURRENT_TIMESTAMP",
                            params![user_id, data],
                        );
                        if let Err(e) = res {
                            warn!("Failed to save goals for user {}: {}", user_id, e);
                        }
                    }
                    SqliteMessage::SaveAskHistory { user_id, data } => {
                        let res = conn.execute(
                            "INSERT INTO ask_history (user_id, data) VALUES (?1, ?2)
                             ON CONFLICT(user_id) DO UPDATE SET data=excluded.data, updated_at=CURRENT_TIMESTAMP",
                            params![user_id, data],
                        );
                        if let Err(e) = res {
                            warn!("Failed to save ask history for user {}: {}", user_id, e);
                        }
                    }
                    SqliteMessage::SaveProactiveCache {
                        user_id,
                        snapshot,
                        analysis,
                    } => {
                        let res = conn.execute(
                            "INSERT INTO proactive_cache (user_id, snapshot, analysis) VALUES (?1, ?2, ?3)
                             ON CONFLICT(user_id) DO UPDATE SET snapshot=excluded.snapshot, analysis=excluded.analysis, updated_at=CURRENT_TIMESTAMP",
                            params![user_id, snapshot, analysis],
                        );
                        if let Err(e) = res {
                            warn!("Failed to save proactive cache for user {}: {}", user_id, e);
                        }
                    }
                }
            }
        });

        Ok(Self {
            tx,
            db_path: db_path.to_string(),
        })
    }

    /// Synchronously initialize the database schema (useful to call at startup before loading data)
    pub fn initialize_schema(db_path: &str) -> anyhow::Result<()> {
        let mut conn = Connection::open(db_path)?;
        init_db(&mut conn)?;
        Ok(())
    }

    /// Read all users synchronously at startup (to populate in-memory store)
    pub fn load_all_users(db_path: &str) -> anyhow::Result<Vec<(u64, Vec<u8>)>> {
        let conn = Connection::open(db_path)?;
        let mut stmt = match conn.prepare("SELECT id, portfolio_data FROM users") {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to prepare SELECT from users table. This might cause portfolio loss! Error: {}", e);
                return Ok(vec![]); // Table probably doesn't exist yet, or DB locked
            }
        };
        let mut rows = stmt.query([])?;
        let mut users = Vec::new();
        while let Some(row) = rows.next()? {
            let id: u64 = row.get(0)?;
            let data: Vec<u8> = row.get(1)?;
            users.push((id, data));
        }
        Ok(users)
    }

    /// Read all users' memos synchronously at startup (dedicated table).
    pub fn load_user_memos(db_path: &str) -> anyhow::Result<Vec<(u64, Vec<crate::models::Memo>)>> {
        let conn = Connection::open(db_path)?;
        let mut stmt = match conn.prepare("SELECT user_id, data FROM user_memos") {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to prepare SELECT from user_memos: {}", e);
                return Ok(vec![]);
            }
        };
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let id: u64 = row.get(0)?;
            let data: String = row.get(1)?;
            if let Ok(list) = serde_json::from_str::<Vec<crate::models::Memo>>(&data) {
                out.push((id, list));
            } else {
                tracing::warn!("Invalid memos JSON for user {}: {}", id, data);
            }
        }
        Ok(out)
    }

    /// Read all users' goals synchronously at startup (dedicated table).
    pub fn load_user_goals(db_path: &str) -> anyhow::Result<Vec<(u64, Vec<crate::models::Goal>)>> {
        let conn = Connection::open(db_path)?;
        let mut stmt = match conn.prepare("SELECT user_id, data FROM user_goals") {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to prepare SELECT from user_goals: {}", e);
                return Ok(vec![]);
            }
        };
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let id: u64 = row.get(0)?;
            let data: String = row.get(1)?;
            if let Ok(list) = serde_json::from_str::<Vec<crate::models::Goal>>(&data) {
                out.push((id, list));
            } else {
                tracing::warn!("Invalid goals JSON for user {}: {}", id, data);
            }
        }
        Ok(out)
    }

    /// Read the /ask Q/A history of a user (oldest first) for multi-turn dialogue.
    pub async fn load_ask_history(&self, user_id: u64) -> anyhow::Result<Vec<(String, String)>> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<(String, String)>> {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare("SELECT data FROM ask_history WHERE user_id = ?1")?;
            let mut rows = stmt.query(params![user_id])?;
            if let Some(row) = rows.next()? {
                let data: String = row.get(0)?;
                Ok(serde_json::from_str::<Vec<(String, String)>>(&data)
                    .unwrap_or_else(|_| Vec::new()))
            } else {
                Ok(Vec::new())
            }
        })
        .await?
    }

    /// Read the last proactive scan context for a user.
    pub async fn load_proactive_cache(
        &self,
        user_id: u64,
    ) -> anyhow::Result<Option<(String, String)>> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut stmt =
                conn.prepare("SELECT snapshot, analysis FROM proactive_cache WHERE user_id = ?1")?;
            let mut rows = stmt.query(params![user_id])?;
            if let Some(row) = rows.next()? {
                let snapshot: String = row.get(0)?;
                let analysis: String = row.get(1)?;
                Ok(Some((snapshot, analysis)))
            } else {
                Ok(None)
            }
        })
        .await?
    }

    /// Asynchronously fire-and-forget a save command
    pub async fn save_user(&self, user_id: u64, data: Vec<u8>) {
        let _ = self
            .tx
            .send(SqliteMessage::SaveUser { user_id, data })
            .await;
    }

    /// Asynchronously fire-and-forget an event log
    #[allow(dead_code)]
    pub async fn log_event(&self, user_id: u64, event_type: &str, data: &str) {
        let _ = self
            .tx
            .send(SqliteMessage::LogEvent {
                user_id,
                event_type: event_type.to_string(),
                data: data.to_string(),
            })
            .await;
    }

    /// Asynchronously fire-and-forget an OHLCV price log (open/high/low/close).
    pub async fn log_ohlcv(&self, symbol: &str, price: f64, open: f64, high: f64, low: f64) {
        let _ = self
            .tx
            .send(SqliteMessage::LogPrice {
                symbol: symbol.to_uppercase(),
                price,
                open: Some(open),
                high: Some(high),
                low: Some(low),
            })
            .await;
    }

    /// Asynchronously fire-and-forget a news summary.
    /// Stored text is normalized (no markdown / emoji): all consumers of these
    /// rows are AI prompt contexts, never rendered messages.
    pub async fn save_news_summary(&self, symbol: &str, summary: &str) {
        let _ = self
            .tx
            .send(SqliteMessage::SaveNewsSummary {
                symbol: symbol.to_uppercase(),
                summary: Self::normalize_cached_text(summary),
            })
            .await;
    }

    /// Append a summary to the per-symbol history (kept for on-demand
    /// "extended results"; retention is handled by [`Self::cleanup_news`]).
    pub async fn save_news_summary_history(&self, symbol: &str, summary: &str) {
        let db_path = self.db_path.clone();
        let symbol = symbol.to_uppercase();
        let summary = Self::normalize_cached_text(summary);
        let _ = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = Connection::open(&db_path)?;
            conn.execute(
                "INSERT INTO news_summary_history (symbol, summary) VALUES (?1, ?2)",
                params![symbol, summary],
            )?;
            Ok(())
        })
        .await;
    }

    /// Asynchronously fire-and-forget a macro summary (normalized the same way
    /// as news summaries; deterministic metric dumps pass through unchanged).
    pub async fn save_macro_summary(&self, symbol: &str, summary: &str) {
        let _ = self
            .tx
            .send(SqliteMessage::SaveMacroSummary {
                symbol: symbol.to_uppercase(),
                summary: Self::normalize_cached_text(summary),
            })
            .await;
    }

    /// Persist the full memo list of a user to its dedicated table.
    pub async fn save_user_memos(&self, user_id: u64, memos: &[crate::models::Memo]) {
        let data = serde_json::to_string(memos).unwrap_or_else(|_| "[]".to_string());
        let _ = self
            .tx
            .send(SqliteMessage::SaveUserMemos { user_id, data })
            .await;
    }

    /// Persist the full goal list of a user to its dedicated table.
    pub async fn save_user_goals(&self, user_id: u64, goals: &[crate::models::Goal]) {
        let data = serde_json::to_string(goals).unwrap_or_else(|_| "[]".to_string());
        let _ = self
            .tx
            .send(SqliteMessage::SaveUserGoals { user_id, data })
            .await;
    }

    /// Persist the full /ask Q/A history of a user (multi-turn dialogue memory).
    pub async fn save_ask_history(&self, user_id: u64, history: &[(String, String)]) {
        let data = serde_json::to_string(history).unwrap_or_else(|_| "[]".to_string());
        let _ = self
            .tx
            .send(SqliteMessage::SaveAskHistory { user_id, data })
            .await;
    }

    /// Persist the last proactive scan context (continuity for the goal-check AI).
    pub async fn save_proactive_cache(&self, user_id: u64, snapshot: &str, analysis: &str) {
        let _ = self
            .tx
            .send(SqliteMessage::SaveProactiveCache {
                user_id,
                snapshot: snapshot.to_string(),
                analysis: analysis.to_string(),
            })
            .await;
    }

    /// Asynchronously fire-and-forget saving quote metadata
    pub async fn save_quote_meta(&self, q: &crate::models::CmcQuote) {
        let _ = self
            .tx
            .send(SqliteMessage::SaveQuoteMeta {
                symbol: q.symbol.to_uppercase(),
                rank: q.rank,
                circulating_supply: q.circulating_supply,
                change_1h: q.change_1h,
                change_24h: q.change_24h,
                change_7d: q.change_7d,
                change_30d: q.change_30d,
                change_90d: q.change_90d,
            })
            .await;
    }

    /// Trigger a cleanup of events older than 30 days
    pub fn cleanup_old_events(&self) {
        let _ = self.tx.try_send(SqliteMessage::CleanupOldEvents);
    }

    pub fn update_model_health(&self, provider_id: &str, status: &str, error_msg: Option<String>) {
        let _ = self.tx.try_send(SqliteMessage::UpdateModelHealth {
            provider_id: provider_id.to_string(),
            status: status.to_string(),
            error_msg,
        });
    }

    pub fn get_model_health(&self, provider_id: &str) -> anyhow::Result<String> {
        let conn = Connection::open(&self.db_path)?;
        let mut stmt = conn.prepare(
            "SELECT status, strftime('%s', last_checked) FROM model_health WHERE provider_id = ?1",
        )?;
        let mut rows = stmt.query(params![provider_id])?;

        if let Some(row) = rows.next()? {
            let status: String = row.get(0)?;
            let last_checked: i64 = row.get(1)?;
            let now = chrono::Utc::now().timestamp();

            // If status is COOLDOWN and 1 hour (3600s) has passed, it's OK — a real
            // failure benches the model for ~1h, then it gets retried by real traffic.
            if status == "COOLDOWN" && (now - last_checked > 3600) {
                return Ok("OK".to_string());
            }
            // DEAD must also expire (24h) — a transient 404/410/network error must
            // never permanently ban a model across restarts/deploys.
            if status == "DEAD" && (now - last_checked > 86400) {
                return Ok("OK".to_string());
            }
            return Ok(status);
        }

        // If not in DB, assume OK
        Ok("OK".to_string())
    }

    /// Read token/request usage for a category in the current calendar month.
    /// Returns (tokens, requests); used for per-provider monthly budgets
    /// (e.g. DeepSeek has no per-key limits, so we enforce them here).
    pub fn get_monthly_usage(&self, category: &str) -> anyhow::Result<(i64, i64)> {
        let conn = Connection::open(&self.db_path)?;
        let month = chrono::Utc::now().format("%Y-%m").to_string();
        let mut stmt = conn.prepare(
            "SELECT tokens, requests FROM monthly_usage WHERE category = ?1 AND month = ?2",
        )?;
        let mut rows = stmt.query(params![category, month])?;
        if let Some(row) = rows.next()? {
            Ok((row.get(0)?, row.get(1)?))
        } else {
            Ok((0, 0))
        }
    }

    /// Add tokens (and one request) to a category's usage for the current month.
    pub fn add_monthly_usage(&self, category: &str, tokens: i64) -> anyhow::Result<()> {
        let conn = Connection::open(&self.db_path)?;
        let month = chrono::Utc::now().format("%Y-%m").to_string();
        conn.execute(
            "INSERT INTO monthly_usage (category, month, tokens, requests) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT(category, month) DO UPDATE SET
                tokens = tokens + excluded.tokens,
                requests = requests + 1",
            params![category, month, tokens],
        )?;
        Ok(())
    }

    /// Record per-model paid usage for today (tokens + 1 request).
    pub fn record_paid_usage(&self, model: &str, tokens: i64) -> anyhow::Result<()> {
        let conn = Connection::open(&self.db_path)?;
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        conn.execute(
            "INSERT INTO paid_usage (model, date, tokens, requests) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT(model, date) DO UPDATE SET
                tokens = tokens + excluded.tokens,
                requests = requests + 1",
            params![model, date, tokens],
        )?;
        Ok(())
    }

    /// Aggregate paid usage per model over the last N days.
    /// Returns (model, tokens, requests).
    pub async fn get_paid_usage_summary(
        &self,
        days: u32,
    ) -> anyhow::Result<Vec<(String, i64, i64)>> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
                "SELECT model, sum(tokens), sum(requests)
                 FROM paid_usage
                 WHERE date >= date('now', ?1)
                 GROUP BY model
                 ORDER BY sum(tokens) DESC",
            )?;
            let modifier = format!("-{} days", days);
            let mut rows = stmt.query(params![modifier])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push((row.get(0)?, row.get(1)?, row.get(2)?));
            }
            Ok(out)
        })
        .await?
    }

    /// Read token/request usage for a category on the current calendar day.
    /// Returns (tokens, requests); used for per-provider daily budgets
    /// (e.g. OpenRouter has a 300 requests/day free-tier cap).
    pub fn get_daily_usage(&self, category: &str) -> anyhow::Result<(i64, i64)> {
        let conn = Connection::open(&self.db_path)?;
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mut stmt = conn.prepare(
            "SELECT tokens, requests FROM daily_usage WHERE category = ?1 AND date = ?2",
        )?;
        let mut rows = stmt.query(params![category, date])?;
        if let Some(row) = rows.next()? {
            Ok((row.get(0)?, row.get(1)?))
        } else {
            Ok((0, 0))
        }
    }

    /// Add tokens (and one request) to a category's usage for the current day.
    pub fn add_daily_usage(&self, category: &str, tokens: i64) -> anyhow::Result<()> {
        let conn = Connection::open(&self.db_path)?;
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        conn.execute(
            "INSERT INTO daily_usage (category, date, tokens, requests) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT(category, date) DO UPDATE SET
                tokens = tokens + excluded.tokens,
                requests = requests + 1",
            params![category, date, tokens],
        )?;
        Ok(())
    }

    /// Upsert a discovered model into the registry (keeps first_seen, bumps last_seen).
    /// `fingerprint` is a provider-supplied change marker (OpenRouter `updated`
    /// timestamp); an empty string means "unknown/never changes".
    /// Used by the model ranker for all providers (OpenRouter discovery + models.json sync).
    pub fn upsert_model_registry(
        &self,
        provider: &str,
        model_id: &str,
        alias: &str,
        display: &str,
        context_length: i64,
        fingerprint: &str,
    ) -> anyhow::Result<()> {
        let conn = Connection::open(&self.db_path)?;
        conn.execute(
            "INSERT INTO model_registry
                (provider, model_id, alias, display, context_length, fingerprint, first_seen, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
             ON CONFLICT(provider, model_id) DO UPDATE SET
                alias = excluded.alias,
                display = excluded.display,
                context_length = excluded.context_length,
                fingerprint = excluded.fingerprint,
                last_seen = CURRENT_TIMESTAMP",
            params![provider, model_id, alias, display, context_length, fingerprint],
        )?;
        Ok(())
    }

    /// All registry entries: (provider, model_id, alias, display, context_length,
    /// fingerprint, last_tested_at, tested_fingerprint, bench_count).
    pub fn get_model_registry(
        &self,
    ) -> anyhow::Result<
        Vec<(
            String,
            String,
            String,
            String,
            i64,
            String,
            Option<String>,
            String,
            i64,
        )>,
    > {
        let conn = Connection::open(&self.db_path)?;
        let mut stmt = conn.prepare(
            "SELECT provider, model_id, alias, display, context_length, fingerprint, last_tested_at, tested_fingerprint, bench_count
             FROM model_registry ORDER BY last_seen DESC",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
            ));
        }
        Ok(out)
    }

    /// Mark a model as tested now (bumps bench_count, stores the fingerprint
    /// that was current at test time so later discovery can detect changes).
    pub fn mark_model_tested(
        &self,
        provider: &str,
        model_id: &str,
        fingerprint: &str,
    ) -> anyhow::Result<()> {
        let conn = Connection::open(&self.db_path)?;
        conn.execute(
            "UPDATE model_registry SET last_tested_at = CURRENT_TIMESTAMP,
                    tested_fingerprint = ?3,
                    bench_count = bench_count + 1
             WHERE provider = ?1 AND model_id = ?2",
            params![provider, model_id, fingerprint],
        )?;
        Ok(())
    }

    /// Record one benchmark/usage datapoint for a model today.
    /// `ok=true` counts a success, `ok=false` counts an error. Quality is
    /// averaged across the day **weighted by bench_calls**: production calls
    /// pass quality=0 (unknown) and never dilute the judge score.
    pub fn record_model_stats(
        &self,
        provider: &str,
        model_id: &str,
        latency_ms: i64,
        ok: bool,
        quality: f64,
    ) -> anyhow::Result<()> {
        let conn = Connection::open(&self.db_path)?;
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let err_inc = if ok { 0 } else { 1 };
        conn.execute(
            "INSERT INTO model_stats
                (provider, model_id, date, calls, errors, total_latency_ms, quality_score, bench_calls)
             VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, CASE WHEN ?6 > 0 THEN 1 ELSE 0 END)
             ON CONFLICT(provider, model_id, date) DO UPDATE SET
                calls = calls + 1,
                errors = errors + excluded.errors,
                total_latency_ms = total_latency_ms + excluded.total_latency_ms,
                bench_calls = bench_calls + excluded.bench_calls,
                quality_score = CASE WHEN excluded.quality_score > 0
                    THEN (quality_score * bench_calls + excluded.quality_score) / (bench_calls + excluded.bench_calls)
                    ELSE quality_score END",
            params![provider, model_id, date, err_inc, latency_ms, quality],
        )?;
        Ok(())
    }

    /// Batch-flush production deltas into model_stats (called by the periodic flusher).
    /// Each delta is (provider, model_id, calls, errors, total_latency_ms).
    #[allow(dead_code)] // planned periodic flusher for the lazy model-ranking feature
    pub fn flush_model_stats(
        &self,
        deltas: Vec<(String, String, i64, i64, i64)>,
    ) -> anyhow::Result<()> {
        let conn = Connection::open(&self.db_path)?;
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        for (provider, model_id, calls, errors, latency) in deltas {
            conn.execute(
                "INSERT INTO model_stats
                    (provider, model_id, date, calls, errors, total_latency_ms, quality_score, bench_calls)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0)
                 ON CONFLICT(provider, model_id, date) DO UPDATE SET
                    calls = calls + excluded.calls,
                    errors = errors + excluded.errors,
                    total_latency_ms = total_latency_ms + excluded.total_latency_ms",
                params![provider, model_id, date, calls, errors, latency],
            )?;
        }
        Ok(())
    }

    /// Aggregate model stats over the last N days.
    /// Returns (provider, model_id, calls, errors, avg_latency_ms, avg_quality).
    pub fn get_model_stats(
        &self,
        days: u32,
    ) -> anyhow::Result<Vec<(String, String, i64, i64, i64, f64)>> {
        let conn = Connection::open(&self.db_path)?;
        let modifier = format!("-{} days", days);
        let mut stmt = conn.prepare(
            "SELECT provider, model_id, sum(calls), sum(errors),
                    sum(total_latency_ms) / max(sum(calls), 1),
                    sum(quality_score * bench_calls) / max(sum(bench_calls), 1)
             FROM model_stats
             WHERE date >= date('now', ?1)
             GROUP BY provider, model_id
             ORDER BY sum(calls) DESC",
        )?;
        let mut rows = stmt.query(params![modifier])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ));
        }
        Ok(out)
    }

    /// Per-model stats for one model over the last N days.
    /// Returns None if the model has no data.
    #[allow(dead_code)] // used by tests; feeds the planned per-model ranking
    pub fn get_model_stats_for(
        &self,
        provider: &str,
        model_id: &str,
        days: u32,
    ) -> anyhow::Result<Option<(i64, i64, i64, f64)>> {
        let conn = Connection::open(&self.db_path)?;
        let modifier = format!("-{} days", days);
        let mut stmt = conn.prepare(
            "SELECT sum(calls), sum(errors),
                    sum(total_latency_ms) / max(sum(calls), 1),
                    sum(quality_score * bench_calls) / max(sum(bench_calls), 1)
             FROM model_stats
             WHERE provider = ?1 AND model_id = ?2 AND date >= date('now', ?3)",
        )?;
        let mut rows = stmt.query(params![provider, model_id, modifier])?;
        if let Some(row) = rows.next()? {
            let calls: i64 = row.get(0)?;
            if calls == 0 {
                return Ok(None);
            }
            Ok(Some((calls, row.get(1)?, row.get(2)?, row.get(3)?)))
        } else {
            Ok(None)
        }
    }

    /// Synchronously query price history from SQLite (spawn blocking thread)
    pub async fn get_price_history(
        &self,
        symbol: &str,
        days: u32,
    ) -> anyhow::Result<Vec<(i64, f64)>> {
        let db_path = self.db_path.clone();
        let symbol = symbol.to_uppercase();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
                "SELECT strftime('%s', timestamp) as ts, price
                 FROM price_history
                 WHERE symbol = ?1 AND timestamp >= datetime('now', ?2)
                 ORDER BY timestamp ASC",
            )?;
            let days_modifier = format!("-{} days", days);
            let mut rows = stmt.query(params![symbol, days_modifier])?;

            let mut history = Vec::new();
            while let Some(row) = rows.next()? {
                let ts: String = row.get(0)?; // strftime('%s') returns a string
                let price: f64 = row.get(1)?;
                if let Ok(ts_int) = ts.parse::<i64>() {
                    history.push((ts_int * 1000, price));
                }
            }
            Ok(history)
        })
        .await?
    }

    /// Synchronously query OHLCV history from SQLite (spawn blocking thread).
    /// Returns `(ts_ms, open, high, low, close)` rows; `open/high/low` are
    /// `None` for legacy rows that predate the OHLCV migration.
    pub async fn get_ohlcv_history(
        &self,
        symbol: &str,
        days: u32,
    ) -> anyhow::Result<Vec<(i64, Option<f64>, Option<f64>, Option<f64>, f64)>> {
        let db_path = self.db_path.clone();
        let symbol = symbol.to_uppercase();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
                "SELECT strftime('%s', timestamp) as ts, open, high, low, price
                 FROM price_history
                 WHERE symbol = ?1 AND timestamp >= datetime('now', ?2)
                 ORDER BY timestamp ASC",
            )?;
            let days_modifier = format!("-{} days", days);
            let mut rows = stmt.query(params![symbol, days_modifier])?;

            let mut history = Vec::new();
            while let Some(row) = rows.next()? {
                let ts: String = row.get(0)?;
                let open: Option<f64> = row.get(1)?;
                let high: Option<f64> = row.get(2)?;
                let low: Option<f64> = row.get(3)?;
                let close: f64 = row.get(4)?;
                if let Ok(ts_int) = ts.parse::<i64>() {
                    history.push((ts_int * 1000, open, high, low, close));
                }
            }
            Ok(history)
        })
        .await?
    }

    /// Synchronously get the latest news summary for a symbol
    pub async fn get_news_summary(&self, symbol: &str) -> anyhow::Result<Option<String>> {
        let db_path = self.db_path.clone();
        let symbol = symbol.to_uppercase();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare("SELECT summary FROM news_summaries WHERE symbol = ?1")?;
            let mut rows = stmt.query(params![symbol])?;
            if let Some(row) = rows.next()? {
                let summary: String = row.get(0)?;
                Ok(Some(summary))
            } else {
                Ok(None)
            }
        })
        .await?
    }

    /// Get the latest news summary for a symbol plus its `updated_at`
    /// timestamp, so callers can reject stale summaries (e.g. the news worker
    /// has been down and the cached text no longer reflects the market).
    pub async fn get_news_summary_with_ts(
        &self,
        symbol: &str,
    ) -> anyhow::Result<Option<(String, String)>> {
        let db_path = self.db_path.clone();
        let symbol = symbol.to_uppercase();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut stmt =
                conn.prepare("SELECT summary, updated_at FROM news_summaries WHERE symbol = ?1")?;
            let mut rows = stmt.query(params![symbol])?;
            if let Some(row) = rows.next()? {
                let summary: String = row.get(0)?;
                let updated_at: String = row.get(1)?;
                Ok(Some((summary, updated_at)))
            } else {
                Ok(None)
            }
        })
        .await?
    }

    /// True when a `news_summaries.updated_at` string is newer than
    /// `max_age_hours`. SQLite stores `CURRENT_TIMESTAMP` as UTC
    /// `YYYY-MM-DD HH:MM:SS`; anything unparseable counts as stale.
    pub fn news_summary_is_fresh(updated_at: &str, max_age_hours: i64) -> bool {
        let Ok(ts) = chrono::NaiveDateTime::parse_from_str(updated_at, "%Y-%m-%d %H:%M:%S") else {
            return false;
        };
        let age = chrono::Utc::now().naive_utc() - ts;
        age.num_hours() <= max_age_hours
    }

    /// Persist raw news articles for a symbol (dedup by url). Kept for
    /// on-demand "extended results" — the AI prompt only ever sees the
    /// summarized form, never this table.
    pub async fn save_news_articles(
        &self,
        symbol: &str,
        articles: &[crate::crypto::providers::cryptocompare::NewsArticle],
    ) -> anyhow::Result<()> {
        let db_path = self.db_path.clone();
        let symbol = symbol.to_uppercase();
        let articles: Vec<(String, String, String, String)> = articles
            .iter()
            .map(|a| {
                (
                    a.source.clone(),
                    a.title.clone(),
                    a.body.clone(),
                    a.url.clone(),
                )
            })
            .collect();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
                "INSERT OR IGNORE INTO news_articles (symbol, source, title, body, url)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (source, title, body, url) in &articles {
                if url.is_empty() {
                    continue;
                }
                stmt.execute(params![symbol, source, title, body, url])?;
            }
            Ok(())
        })
        .await?
    }

    /// Raw articles for a symbol over the last `days` (extended results).
    pub async fn get_news_articles(
        &self,
        symbol: &str,
        days: i64,
    ) -> anyhow::Result<Vec<(String, String, String, String, String)>> {
        let db_path = self.db_path.clone();
        let symbol = symbol.to_uppercase();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<_>> {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
                "SELECT source, title, body, url, fetched_at FROM news_articles
                 WHERE symbol = ?1 AND fetched_at >= datetime('now', ?2)
                 ORDER BY fetched_at DESC LIMIT 50",
            )?;
            let modifier = format!("-{days} days");
            let rows = stmt.query_map(params![symbol, modifier], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await?
    }

    /// Recent summary history for a symbol (extended results).
    pub async fn list_news_summary_history(
        &self,
        symbol: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let db_path = self.db_path.clone();
        let symbol = symbol.to_uppercase();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<_>> {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
                "SELECT summary, created_at FROM news_summary_history
                 WHERE symbol = ?1 ORDER BY created_at DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![symbol, limit], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await?
    }

    /// Retention sweep: drop raw articles older than `article_days` and
    /// summary history older than `history_days`. Returns rows deleted.
    pub async fn cleanup_news(&self, article_days: i64, history_days: i64) -> anyhow::Result<i64> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<i64> {
            let conn = Connection::open(&db_path)?;
            let a = conn.execute(
                "DELETE FROM news_articles WHERE fetched_at < datetime('now', ?1)",
                params![format!("-{article_days} days")],
            )?;
            let h = conn.execute(
                "DELETE FROM news_summary_history WHERE created_at < datetime('now', ?1)",
                params![format!("-{history_days} days")],
            )?;
            Ok((a + h) as i64)
        })
        .await?
    }

    /// Get the latest macro/market-intel summary for a key.
    /// Returns `None` if the entry is absent OR older than `max_age_hours`.
    pub async fn get_macro_summary(
        &self,
        symbol: &str,
        max_age_hours: i64,
    ) -> anyhow::Result<Option<String>> {
        let db_path = self.db_path.clone();
        let symbol = symbol.to_uppercase();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
                "SELECT summary, strftime('%s', updated_at) FROM macro_cache WHERE symbol = ?1",
            )?;
            let mut rows = stmt.query(params![symbol])?;
            if let Some(row) = rows.next()? {
                let summary: String = row.get(0)?;
                let updated_ts: Option<String> = row.get(1).unwrap_or(None);
                if let Some(ts_str) = updated_ts {
                    if let Ok(ts) = ts_str.parse::<i64>() {
                        let age_secs = chrono::Utc::now().timestamp() - ts;
                        if age_secs > max_age_hours * 3600 {
                            return Ok(None); // Expired — let the worker regenerate
                        }
                    }
                }
                Ok(Some(summary))
            } else {
                Ok(None)
            }
        })
        .await?
    }

    /// Synchronously get the cached metadata for a symbol
    pub async fn get_quote_meta(
        &self,
        symbol: &str,
    ) -> anyhow::Result<Option<crate::models::CmcQuote>> {
        let db_path = self.db_path.clone();
        let symbol = symbol.to_uppercase();
        let name = symbol.clone(); // Will just use symbol as name if retrieved from cache
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
                "SELECT rank, circulating_supply, change_1h, change_24h, change_7d, change_30d, change_90d, changes_updated_at
                 FROM quote_meta_cache WHERE symbol = ?1"
            )?;
            let mut rows = stmt.query(params![symbol])?;
            if let Some(row) = rows.next()? {
                let rank: u32 = row.get(0)?;
                let circulating_supply: f64 = row.get(1)?;

                // Read cached changes if they are recent enough (e.g. less than 6 hours old)
                let mut change_1h: Option<f64> = row.get(2).unwrap_or(None);
                let mut change_24h: Option<f64> = row.get(3).unwrap_or(None);
                let mut change_7d: Option<f64> = row.get(4).unwrap_or(None);
                let mut change_30d: Option<f64> = row.get(5).unwrap_or(None);
                let mut change_90d: Option<f64> = row.get(6).unwrap_or(None);

                let changes_updated_at: Option<String> = row.get(7).unwrap_or(None);
                if let Some(ts_str) = changes_updated_at {
                    if let Ok(ts) = ts_str.parse::<chrono::DateTime<chrono::Utc>>() {
                        let age = chrono::Utc::now() - ts;
                        if age.num_hours() > 6 {
                            // Cache is too old, invalidate changes
                            change_1h = None;
                            change_24h = None;
                            change_7d = None;
                            change_30d = None;
                            change_90d = None;
                        }
                    }
                }

                Ok(Some(crate::models::CmcQuote {
                    symbol,
                    name,
                    price_usd: 0.0, // This is just metadata cache, price comes from live
                    market_cap: 0.0,
                    volume_24h: 0.0,
                    change_1h,
                    change_24h,
                    change_7d,
                    change_30d,
                    change_90d,
                    change_365d: None,
                    change_730d: None,
                    ath: None,
                    ath_change_percentage: None,
                    high_24h: None,
                    low_24h: None,
                    rank,
                    circulating_supply,
                    last_updated: chrono::Utc::now(),
                    anomaly_warning: None,
                }))
            } else {
                Ok(None)
            }
        }).await?
    }

    /// Asynchronously flush daily metrics deltas
    pub async fn flush_daily_metrics(&self, metrics: Vec<crate::models::MetricDelta>) {
        let _ = self
            .tx
            .send(SqliteMessage::FlushDailyMetrics { metrics })
            .await;
    }

    /// Fire-and-forget: save a concrete price milestone for a symbol
    pub async fn save_price_milestone(
        &self,
        symbol: &str,
        milestone: &str,
        price: f64,
        recorded_at: &str,
    ) {
        let _ = self
            .tx
            .send(SqliteMessage::SavePriceMilestone {
                symbol: symbol.to_uppercase(),
                milestone: milestone.to_string(),
                price,
                recorded_at: recorded_at.to_string(),
            })
            .await;
    }

    /// Read all price milestones for a symbol: returns (milestone_name, price, recorded_at)
    pub async fn get_price_milestones(
        &self,
        symbol: &str,
    ) -> anyhow::Result<Vec<(String, f64, String)>> {
        let db_path = self.db_path.clone();
        let symbol = symbol.to_uppercase();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
                "SELECT milestone, price, recorded_at, updated_at FROM price_milestones
                 WHERE symbol = ?1",
            )?;
            let mut rows = stmt.query(params![symbol])?;
            let mut result = Vec::new();
            while let Some(row) = rows.next()? {
                let milestone: String = row.get(0)?;
                let price: f64 = row.get(1)?;
                let recorded_at: String = row.get(2)?;
                result.push((milestone, price, recorded_at));
            }
            Ok(result)
        })
        .await?
    }

    /// Check which symbols are missing milestones (needed for startup warming)
    pub async fn get_symbols_without_milestones(&self) -> anyhow::Result<Vec<String>> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            // Return symbols in quote_meta_cache that have no entries in price_milestones
            let mut stmt = conn.prepare(
                "SELECT q.symbol FROM quote_meta_cache q
                 WHERE NOT EXISTS (
                     SELECT 1 FROM price_milestones p WHERE p.symbol = q.symbol
                 )
                 ORDER BY q.rank ASC
                 LIMIT 20",
            )?;
            let mut rows = stmt.query([])?;
            let mut symbols = Vec::new();
            while let Some(row) = rows.next()? {
                let sym: String = row.get(0)?;
                symbols.push(sym);
            }
            Ok(symbols)
        })
        .await?
    }

    /// Reset the bot's "knowledge base": wipe all derived/cache data (news
    /// summaries, macro cache, price milestones, model health, stats, quotes
    /// meta, local history) while KEEPING user data (portfolios, goals, memos,
    /// events) and the paid-usage budget counters. Use after a poisoned-data
    /// incident or when you want a clean slate.
    pub fn wipe_knowledge(&self) -> anyhow::Result<usize> {
        let conn = Connection::open(&self.db_path)?;
        let tables = [
            "news_summaries",
            "news_articles",
            "news_summary_history",
            "quote_meta_cache",
            "model_health",
            "macro_cache",
            "price_milestones",
            "proactive_cache",
            "ai_stats",
            "daily_metrics",
            "price_history",
        ];
        let mut total = 0usize;
        for t in tables {
            total += conn.execute(&format!("DELETE FROM {}", t), [])?;
        }
        Ok(total)
    }

    /// Delete stats rows whose source no longer exists — e.g. models removed
    /// from models.json on upgrade. Without this, old rows (Vercel, dead
    /// Gemini models, …) stay in the stats forever after an update.
    /// Returns the number of deleted rows.
    pub fn prune_stale_metrics(&self, valid_items: &[String]) -> anyhow::Result<usize> {
        let conn = Connection::open(&self.db_path)?;
        let category = "AI Model";
        let n = if valid_items.is_empty() {
            conn.execute(
                "DELETE FROM daily_metrics WHERE category = ?1",
                params![category],
            )?
        } else {
            let placeholders = vec!["?"; valid_items.len()].join(",");
            let sql = format!(
                "DELETE FROM daily_metrics WHERE category = ?1 AND item NOT IN ({})",
                placeholders
            );
            let mut params: Vec<&dyn rusqlite::ToSql> = vec![&category];
            params.extend(valid_items.iter().map(|s| s as &dyn rusqlite::ToSql));
            conn.execute(&sql, &params[..])?
        };
        Ok(n)
    }

    /// Synchronously get aggregated metrics for the last N days
    pub async fn get_aggregated_metrics(
        &self,
        days: u32,
    ) -> anyhow::Result<Vec<crate::models::MetricAgg>> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
                "SELECT category, item, sum(ok_count), sum(err_count)
                 FROM daily_metrics
                 WHERE date >= date('now', ?1)
                 GROUP BY category, item",
            )?;
            let days_modifier = format!("-{} days", days);
            let mut rows = stmt.query(params![days_modifier])?;

            let mut results = Vec::new();
            while let Some(row) = rows.next()? {
                results.push(crate::models::MetricAgg {
                    category: row.get(0)?,
                    item: row.get(1)?,
                    ok_count: row.get(2)?,
                    err_count: row.get(3)?,
                });
            }
            Ok(results)
        })
        .await?
    }
}

fn init_db(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA max_page_count = 32768;
        ",
    )?;

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY,
            portfolio_data BLOB NOT NULL,
            last_updated DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            event_type TEXT NOT NULL,
            data TEXT NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE INDEX IF NOT EXISTS idx_events_user ON events(user_id);

        CREATE TABLE IF NOT EXISTS price_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            symbol TEXT NOT NULL,
            price REAL NOT NULL,
            open REAL,
            high REAL,
            low REAL,
            timestamp DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE INDEX IF NOT EXISTS idx_price_history_sym_ts ON price_history(symbol, timestamp);

        CREATE TABLE IF NOT EXISTS news_summaries (
            symbol TEXT PRIMARY KEY,
            summary TEXT NOT NULL,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS news_articles (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            symbol TEXT NOT NULL,
            source TEXT NOT NULL,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            url TEXT NOT NULL,
            fetched_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_news_articles_sym_ts
            ON news_articles(symbol, fetched_at);

        CREATE TABLE IF NOT EXISTS news_summary_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            symbol TEXT NOT NULL,
            summary TEXT NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_news_history_sym_ts
            ON news_summary_history(symbol, created_at);

        CREATE TABLE IF NOT EXISTS quote_meta_cache (
            symbol TEXT PRIMARY KEY,
            rank INTEGER NOT NULL,
            circulating_supply REAL NOT NULL,
            change_1h REAL,
            change_24h REAL,
            change_7d REAL,
            change_30d REAL,
            change_90d REAL,
            changes_updated_at DATETIME,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS model_health (
            provider_id TEXT PRIMARY KEY,
            status TEXT NOT NULL,
            error_msg TEXT,
            last_checked DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS macro_cache (
            symbol TEXT PRIMARY KEY,
            summary TEXT NOT NULL,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS price_milestones (
            symbol TEXT NOT NULL,
            milestone TEXT NOT NULL,
            price REAL NOT NULL,
            recorded_at TEXT NOT NULL,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (symbol, milestone)
        );

        CREATE TABLE IF NOT EXISTS monthly_usage (
            category TEXT NOT NULL,
            month TEXT NOT NULL,
            tokens INTEGER NOT NULL DEFAULT 0,
            requests INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (category, month)
        );

        CREATE TABLE IF NOT EXISTS paid_usage (
            model TEXT NOT NULL,
            date TEXT NOT NULL,
            tokens INTEGER NOT NULL DEFAULT 0,
            requests INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (model, date)
        );

        CREATE TABLE IF NOT EXISTS user_memos (
            user_id INTEGER PRIMARY KEY,
            data TEXT NOT NULL,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS user_goals (
            user_id INTEGER PRIMARY KEY,
            data TEXT NOT NULL,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS ask_history (
            user_id INTEGER PRIMARY KEY,
            data TEXT NOT NULL,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS proactive_cache (
            user_id INTEGER PRIMARY KEY,
            snapshot TEXT,
            analysis TEXT,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS daily_usage (
            category TEXT NOT NULL,
            date TEXT NOT NULL,
            tokens INTEGER NOT NULL DEFAULT 0,
            requests INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (category, date)
        );

        CREATE TABLE IF NOT EXISTS model_registry (
            provider TEXT NOT NULL,
            model_id TEXT NOT NULL,
            alias TEXT NOT NULL,
            display TEXT NOT NULL,
            context_length INTEGER NOT NULL DEFAULT 0,
            fingerprint TEXT NOT NULL DEFAULT '',
            first_seen TEXT NOT NULL,
            last_seen TEXT NOT NULL,
            last_tested_at TEXT,
            tested_fingerprint TEXT NOT NULL DEFAULT '',
            bench_count INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (provider, model_id)
        );

        CREATE TABLE IF NOT EXISTS model_stats (
            provider TEXT NOT NULL,
            model_id TEXT NOT NULL,
            date TEXT NOT NULL,
            calls INTEGER NOT NULL DEFAULT 0,
            errors INTEGER NOT NULL DEFAULT 0,
            total_latency_ms INTEGER NOT NULL DEFAULT 0,
            quality_score REAL NOT NULL DEFAULT 0,
            bench_calls INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (provider, model_id, date)
        );
        ",
    )?;

    // Safely add columns to existing tables (ignored if they already exist)
    let _ = conn.execute(
        "ALTER TABLE quote_meta_cache ADD COLUMN change_1h REAL;",
        (),
    );
    let _ = conn.execute(
        "ALTER TABLE quote_meta_cache ADD COLUMN change_24h REAL;",
        (),
    );
    let _ = conn.execute(
        "ALTER TABLE quote_meta_cache ADD COLUMN change_7d REAL;",
        (),
    );
    let _ = conn.execute(
        "ALTER TABLE quote_meta_cache ADD COLUMN change_30d REAL;",
        (),
    );
    let _ = conn.execute(
        "ALTER TABLE quote_meta_cache ADD COLUMN change_90d REAL;",
        (),
    );
    let _ = conn.execute(
        "ALTER TABLE quote_meta_cache ADD COLUMN changes_updated_at DATETIME;",
        (),
    );

    // Migration for existing DBs: OHLCV columns on price_history. Older
    // schemas only had (symbol, price, timestamp); without these columns the
    // price-logging INSERT fails and the SQLite history fallback stays empty.
    let _ = conn.execute("ALTER TABLE price_history ADD COLUMN open REAL;", ());
    let _ = conn.execute("ALTER TABLE price_history ADD COLUMN high REAL;", ());
    let _ = conn.execute("ALTER TABLE price_history ADD COLUMN low REAL;", ());

    // Migration for existing DBs: model fingerprint (OpenRouter `updated` field)
    // lets the lazy ranker skip re-testing models that have not changed.
    let _ = conn.execute(
        "ALTER TABLE model_registry ADD COLUMN fingerprint TEXT NOT NULL DEFAULT '';",
        (),
    );
    let _ = conn.execute(
        "ALTER TABLE model_registry ADD COLUMN tested_fingerprint TEXT NOT NULL DEFAULT '';",
        (),
    );
    // Migration for existing DBs: bench_calls tracks only quality-bearing
    // benchmark calls so production traffic (quality=0) cannot dilute the
    // average judge score used for role assignment.
    let _ = conn.execute(
        "ALTER TABLE model_stats ADD COLUMN bench_calls INTEGER NOT NULL DEFAULT 0;",
        (),
    );

    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS ai_stats (
            model TEXT PRIMARY KEY,
            ok_count INTEGER NOT NULL DEFAULT 0,
            err_count INTEGER NOT NULL DEFAULT 0
        );",
        (),
    );

    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS daily_metrics (
            category TEXT NOT NULL,
            item TEXT NOT NULL,
            date TEXT NOT NULL,
            ok_count INTEGER NOT NULL DEFAULT 0,
            err_count INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (category, item, date)
        );",
        (),
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> SqliteStore {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "cryptograph_sqlite_test_{}_{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        SqliteStore::initialize_schema(&path.to_string_lossy()).unwrap();
        // `new` spawns a blocking worker via tokio, so it needs a runtime context.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let store = rt.block_on(async { SqliteStore::new(&path.to_string_lossy()).unwrap() });
        std::mem::forget(rt); // keep the worker alive for the duration of the test
        store
    }

    #[test]
    fn model_stats_quality_isolated_from_production_calls() {
        let store = tmp_store();
        // Production traffic records quality=0 and must not dilute the judge score.
        store
            .record_model_stats("Groq", "gpt-oss-120b", 900, true, 0.0)
            .unwrap();
        store
            .record_model_stats("Groq", "gpt-oss-120b", 800, true, 0.0)
            .unwrap();
        store
            .record_model_stats("Groq", "gpt-oss-120b", 700, false, 0.0)
            .unwrap();
        // One benchmark call with a real judge score.
        store
            .record_model_stats("Groq", "gpt-oss-120b", 1000, true, 9.0)
            .unwrap();

        let rows = store.get_model_stats(14).unwrap();
        let (_, _, calls, errors, _, quality) = rows
            .iter()
            .find(|(p, m, _, _, _, _)| p == "Groq" && m == "gpt-oss-120b")
            .expect("stats row present");
        assert_eq!(*calls, 4);
        assert_eq!(*errors, 1);
        assert_eq!(
            *quality, 9.0,
            "production calls must not dilute bench quality"
        );

        // A second benchmark call averages with the first (9.0 + 7.0) / 2 = 8.0.
        store
            .record_model_stats("Groq", "gpt-oss-120b", 1100, true, 7.0)
            .unwrap();
        let rows = store.get_model_stats(14).unwrap();
        let (_, _, _, _, _, quality) = rows
            .iter()
            .find(|(p, m, _, _, _, _)| p == "Groq" && m == "gpt-oss-120b")
            .expect("stats row present");
        assert_eq!(*quality, 8.0);
    }

    #[test]
    fn ask_history_roundtrip() {
        let store = tmp_store();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            store
                .save_ask_history(
                    42,
                    &[
                        ("q1".to_string(), "a1".to_string()),
                        ("q2".to_string(), "a2".to_string()),
                    ],
                )
                .await;
        });
        // The worker thread processes the channel asynchronously.
        std::thread::sleep(std::time::Duration::from_millis(150));
        let loaded = rt.block_on(async { store.load_ask_history(42).await.unwrap() });
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].0, "q1");
        assert_eq!(loaded[0].1, "a1");
        assert_eq!(loaded[1].0, "q2");
        assert_eq!(loaded[1].1, "a2");
        // Unknown user -> empty, never an error.
        let empty = rt.block_on(async { store.load_ask_history(999).await.unwrap() });
        assert!(empty.is_empty());
    }

    #[test]
    fn model_stats_per_model_view_matches_aggregate() {
        let store = tmp_store();
        store
            .record_model_stats("Cohere", "command-a-reasoning", 5000, true, 8.5)
            .unwrap();
        store
            .record_model_stats("Cohere", "command-a-reasoning", 4000, true, 0.0)
            .unwrap();
        let per = store
            .get_model_stats_for("Cohere", "command-a-reasoning", 14)
            .unwrap()
            .expect("per-model row");
        assert_eq!(per.0, 2, "calls");
        assert_eq!(per.1, 0, "errors");
        assert_eq!(per.3, 8.5, "quality from bench only");
    }
}
