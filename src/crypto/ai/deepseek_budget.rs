use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Hard monthly budget for the paid DeepSeek provider (which has no per-key limits).
///
/// Guarantees:
/// - **Never exceeds the budget**: a request slot and a conservative token
///   estimate are reserved atomically with `fetch_add` + rollback, so parallel
///   callers cannot overshoot the cap without a lock (a lock held across a
///   30-120s request was what starved the critic).
/// - **Survives SQLite failures**: counters are mirrored in RAM. If the DB is
///   unavailable, the RAM copy stays authoritative for the rest of the month
///   (and is re-synced from the DB on the next month / restart if DB recovers).
pub struct DeepSeekBudget {
    /// Weak handle so short-lived CLI processes can drop the store (and its
    /// SQLite worker thread) and exit cleanly; the long-running bot keeps the
    /// Arc alive, so upgrades always succeed there.
    db: Mutex<Option<std::sync::Weak<crate::db::sqlite::SqliteStore>>>,
    /// Current calendar month as YYYYMM (counters below belong to it).
    month: AtomicI64,
    tokens_used: AtomicI64,
    requests_used: AtomicI64,
    /// Sticky flag: once DB ops fail, keep counting purely in RAM this month.
    db_failed: AtomicBool,
    /// Unix second of the last DB hydration; throttles `refresh()` so the
    /// hot path never does a SQLite read (and never needs a call lock).
    last_refresh: AtomicI64,
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
            last_refresh: AtomicI64::new(0),
        })
    }

    /// Wire up the SQLite handle at startup. If `None` is passed (no DB available),
    /// the budget is enforced purely in RAM.
    pub fn init(&self, db: Option<Arc<crate::db::sqlite::SqliteStore>>) {
        *self.db.lock().unwrap() = db.map(|d| Arc::downgrade(&d));
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
    /// Hydration is throttled and uses `fetch_max`, so a concurrent `reserve`
    /// can never have its increments rolled back by a stale DB read.
    fn refresh(&self) {
        let m = Self::month_now();
        if self.month.swap(m, Ordering::SeqCst) != m {
            self.tokens_used.store(0, Ordering::SeqCst);
            self.requests_used.store(0, Ordering::SeqCst);
            self.db_failed.store(false, Ordering::SeqCst);
            self.last_refresh.store(0, Ordering::SeqCst);
        }
        if self.db_failed.load(Ordering::SeqCst) {
            return;
        }
        let now = chrono::Utc::now().timestamp();
        let last = self.last_refresh.load(Ordering::SeqCst);
        if now.saturating_sub(last) < 30 {
            return;
        }
        self.last_refresh.store(now, Ordering::SeqCst);
        let db = self.db.lock().unwrap().clone();
        if let Some(weak) = db {
            if let Some(store) = weak.upgrade() {
                match store.get_monthly_usage("deepseek") {
                    Ok((t, r)) => {
                        self.tokens_used.fetch_max(t, Ordering::SeqCst);
                        self.requests_used.fetch_max(r, Ordering::SeqCst);
                    }
                    Err(e) => {
                        tracing::warn!("DeepSeek budget: DB read failed, keeping RAM copy: {}", e);
                        self.db_failed.store(true, Ordering::SeqCst);
                    }
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
            .unwrap_or(500)
    }

    /// Atomically reserve one request slot plus a conservative token estimate.
    /// Returns the reserved estimate, which [`Self::settle`] must release.
    /// Safe to call from parallel tasks: no lock, and both caps are enforced
    /// with fetch_add + rollback, so the month can never overshoot.
    pub fn reserve(&self, prompt: &str) -> Result<i64, String> {
        self.refresh();

        // Conservative upper bound for the whole call: prompt tokens (chars/2 is an
        // upper bound even for Cyrillic), max output tokens, plus safety margin.
        let est = (prompt.len() as i64 / 2) + 8192 + 1024;
        let after_t = self.tokens_used.fetch_add(est, Ordering::SeqCst) + est;
        if after_t > Self::token_budget() {
            self.tokens_used.fetch_sub(est, Ordering::SeqCst);
            return Err(format!(
                "monthly token budget would be exceeded ({} + ~{} > {})",
                after_t - est,
                est,
                Self::token_budget()
            ));
        }

        let used_r = self.requests_used.fetch_add(1, Ordering::SeqCst) + 1;
        if used_r > Self::req_budget() {
            self.requests_used.fetch_sub(1, Ordering::SeqCst);
            self.tokens_used.fetch_sub(est, Ordering::SeqCst);
            return Err(format!(
                "monthly request budget exceeded ({} / {})",
                used_r - 1,
                Self::req_budget()
            ));
        }
        Ok(est)
    }

    /// Release the reservation from [`Self::reserve`] and commit the actual usage.
    pub fn settle(&self, reserved: i64, tokens: i64) {
        let t = tokens.max(1);
        self.tokens_used.fetch_sub(reserved, Ordering::SeqCst);
        self.tokens_used.fetch_add(t, Ordering::SeqCst);
        if self.db_failed.load(Ordering::SeqCst) {
            return;
        }
        let db = self.db.lock().unwrap().clone();
        if let Some(weak) = db {
            if let Some(store) = weak.upgrade() {
                if let Err(e) = store.add_monthly_usage("deepseek", t) {
                    tracing::warn!(
                        "DeepSeek budget: DB write failed, RAM copy stays authoritative: {}",
                        e
                    );
                    self.db_failed.store(true, Ordering::SeqCst);
                }
            }
        }
    }

    /// Record per-model daily usage for the /stats report (best-effort, non-critical).
    pub fn record_daily(&self, model: &str, tokens: i64) {
        if self.db_failed.load(Ordering::SeqCst) {
            return;
        }
        let db = self.db.lock().unwrap().clone();
        if let Some(weak) = db {
            if let Some(store) = weak.upgrade() {
                let _ = store.record_paid_usage(model, tokens.max(1));
            }
        }
    }
}
