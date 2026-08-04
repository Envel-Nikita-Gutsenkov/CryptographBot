use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Hard monthly budget for the paid DeepSeek provider (which has no per-key limits).
///
/// Guarantees:
/// - **Never exceeds the budget**: DeepSeek calls are serialized by a global call
///   lock in `call_deepseek_model`; a request slot is consumed atomically and the
///   token check uses a conservative upper bound (prompt estimate + max output),
///   so even a single large response cannot push the month over the limit.
/// - **Survives SQLite failures**: counters are mirrored in RAM. If the DB is
///   unavailable, the RAM copy stays authoritative for the rest of the month
///   (and is re-synced from the DB on the next month / restart if DB recovers).
pub struct DeepSeekBudget {
    db: Mutex<Option<Arc<crate::db::sqlite::SqliteStore>>>,
    /// Current calendar month as YYYYMM (counters below belong to it).
    month: AtomicI64,
    tokens_used: AtomicI64,
    requests_used: AtomicI64,
    /// Sticky flag: once DB ops fail, keep counting purely in RAM this month.
    db_failed: AtomicBool,
}

static INSTANCE: OnceLock<DeepSeekBudget> = OnceLock::new();

impl DeepSeekBudget {
    pub fn instance() -> &'static Self {
        INSTANCE.get_or_init(|| Self {
            db: Mutex::new(None),
            month: AtomicI64::new(0),
            tokens_used: AtomicI64::new(0),
            requests_used: AtomicI64::new(0),
            db_failed: AtomicBool::new(false),
        })
    }

    /// Wire up the SQLite handle at startup. If `None` is passed (no DB available),
    /// the budget is enforced purely in RAM.
    pub fn init(&self, db: Option<Arc<crate::db::sqlite::SqliteStore>>) {
        *self.db.lock().unwrap() = db;
        self.refresh();
    }

    fn month_now() -> i64 {
        chrono::Utc::now()
            .format("%Y%m")
            .to_string()
            .parse()
            .unwrap_or(0)
    }

    /// Re-sync RAM counters: resets on month rollover, hydrates from DB otherwise.
    fn refresh(&self) {
        let m = Self::month_now();
        if self.month.swap(m, Ordering::SeqCst) != m {
            self.tokens_used.store(0, Ordering::SeqCst);
            self.requests_used.store(0, Ordering::SeqCst);
            self.db_failed.store(false, Ordering::SeqCst);
        }
        if self.db_failed.load(Ordering::SeqCst) {
            return;
        }
        let db = self.db.lock().unwrap().clone();
        if let Some(store) = db {
            match store.get_monthly_usage("deepseek") {
                Ok((t, r)) => {
                    self.tokens_used.store(t, Ordering::SeqCst);
                    self.requests_used.store(r, Ordering::SeqCst);
                }
                Err(e) => {
                    tracing::warn!("DeepSeek budget: DB read failed, keeping RAM copy: {}", e);
                    self.db_failed.store(true, Ordering::SeqCst);
                }
            }
        }
    }

    fn token_budget() -> i64 {
        std::env::var("DEEPSEEK_MONTHLY_TOKEN_BUDGET")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3_000_000)
    }

    fn req_budget() -> i64 {
        std::env::var("DEEPSEEK_MONTHLY_REQ_BUDGET")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(150)
    }

    /// Atomically consume one request slot and verify the estimated token cost fits
    /// the remaining monthly budget. Returns `Err` if the call would exceed either limit.
    /// Must be called while holding the DeepSeek call lock.
    pub fn reserve(&self, prompt: &str) -> Result<(), String> {
        self.refresh();

        let used_t = self.tokens_used.load(Ordering::SeqCst);
        // Conservative upper bound for the whole call: prompt tokens (chars/2 is an
        // upper bound even for Cyrillic), max output tokens, plus safety margin.
        let est = (prompt.len() as i64 / 2) + 8192 + 1024;
        if used_t + est > Self::token_budget() {
            return Err(format!(
                "monthly token budget would be exceeded ({} + ~{} > {})",
                used_t,
                est,
                Self::token_budget()
            ));
        }

        let used_r = self.requests_used.fetch_add(1, Ordering::SeqCst) + 1;
        if used_r > Self::req_budget() {
            self.requests_used.fetch_sub(1, Ordering::SeqCst);
            return Err(format!(
                "monthly request budget exceeded ({} / {})",
                used_r - 1,
                Self::req_budget()
            ));
        }
        Ok(())
    }

    /// Commit the actual tokens used by a successful call (RAM + best-effort DB).
    pub fn settle(&self, tokens: i64) {
        let t = tokens.max(1);
        self.tokens_used.fetch_add(t, Ordering::SeqCst);
        if self.db_failed.load(Ordering::SeqCst) {
            return;
        }
        let db = self.db.lock().unwrap().clone();
        if let Some(store) = db {
            if let Err(e) = store.add_monthly_usage("deepseek", t) {
                tracing::warn!(
                    "DeepSeek budget: DB write failed, RAM copy stays authoritative: {}",
                    e
                );
                self.db_failed.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Record per-model daily usage for the /stats report (best-effort, non-critical).
    pub fn record_daily(&self, model: &str, tokens: i64) {
        if self.db_failed.load(Ordering::SeqCst) {
            return;
        }
        let db = self.db.lock().unwrap().clone();
        if let Some(store) = db {
            let _ = store.record_paid_usage(model, tokens.max(1));
        }
    }
}
