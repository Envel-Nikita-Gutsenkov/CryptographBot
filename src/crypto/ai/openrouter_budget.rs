use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Hard daily budget for the free OpenRouter tier (300 requests/day on the
/// shared key). OpenRouter free models have no per-model rate limit, so the
/// cap is enforced here, exactly like `DeepSeekBudget` does for DeepSeek.
///
/// Guarantees:
/// - **Never exceeds the daily cap**: OpenRouter calls are serialized by a
///   global call lock in `call_openrouter_model`; a request slot is consumed
///   atomically, so concurrent bursts cannot overshoot the limit.
/// - **Survives SQLite failures**: counters are mirrored in RAM. If the DB is
///   unavailable, the RAM copy stays authoritative for the rest of the day.
pub struct OpenRouterBudget {
    /// Weak handle so short-lived CLI processes can drop the store (and its
    /// SQLite worker thread) and exit cleanly; the long-running bot keeps the
    /// Arc alive, so upgrades always succeed there.
    db: Mutex<Option<std::sync::Weak<crate::db::sqlite::SqliteStore>>>,
    /// Current calendar day as YYYY-MM-DD (counters below belong to it).
    day: AtomicI64,
    tokens_used: AtomicI64,
    requests_used: AtomicI64,
    /// Sticky flag: once DB ops fail, keep counting purely in RAM this day.
    db_failed: AtomicBool,
}

static INSTANCE: OnceLock<OpenRouterBudget> = OnceLock::new();

impl OpenRouterBudget {
    pub fn instance() -> &'static Self {
        INSTANCE.get_or_init(|| Self {
            db: Mutex::new(None),
            day: AtomicI64::new(0),
            tokens_used: AtomicI64::new(0),
            requests_used: AtomicI64::new(0),
            db_failed: AtomicBool::new(false),
        })
    }

    /// Wire up the SQLite handle at startup. If `None` is passed (no DB available),
    /// the budget is enforced purely in RAM.
    pub fn init(&self, db: Option<Arc<crate::db::sqlite::SqliteStore>>) {
        *self.db.lock().unwrap() = db.map(|d| Arc::downgrade(&d));
        self.refresh();
    }

    fn day_now() -> i64 {
        chrono::Utc::now()
            .format("%Y%m%d")
            .to_string()
            .parse()
            .unwrap_or(0)
    }

    /// Re-sync RAM counters: resets on day rollover, hydrates from DB otherwise.
    fn refresh(&self) {
        let d = Self::day_now();
        if self.day.swap(d, Ordering::SeqCst) != d {
            self.tokens_used.store(0, Ordering::SeqCst);
            self.requests_used.store(0, Ordering::SeqCst);
            self.db_failed.store(false, Ordering::SeqCst);
        }
        if self.db_failed.load(Ordering::SeqCst) {
            return;
        }
        let db = self.db.lock().unwrap().clone();
        if let Some(weak) = db {
            if let Some(store) = weak.upgrade() {
                match store.get_daily_usage("openrouter") {
                    Ok((t, r)) => {
                        self.tokens_used.store(t, Ordering::SeqCst);
                        self.requests_used.store(r, Ordering::SeqCst);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "OpenRouter budget: DB read failed, keeping RAM copy: {}",
                            e
                        );
                        self.db_failed.store(true, Ordering::SeqCst);
                    }
                }
            }
        }
    }

    fn req_budget() -> i64 {
        std::env::var("OPENROUTER_DAILY_REQ_BUDGET")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300)
    }

    /// Atomically consume one request slot. Returns `Err` if the daily cap
    /// would be exceeded. Must be called while holding the OpenRouter call lock.
    pub fn reserve(&self) -> Result<(), String> {
        self.refresh();

        let used_r = self.requests_used.fetch_add(1, Ordering::SeqCst) + 1;
        if used_r > Self::req_budget() {
            self.requests_used.fetch_sub(1, Ordering::SeqCst);
            return Err(format!(
                "daily request budget exceeded ({} / {})",
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
        if let Some(weak) = db {
            if let Some(store) = weak.upgrade() {
                if let Err(e) = store.add_daily_usage("openrouter", t) {
                    tracing::warn!(
                        "OpenRouter budget: DB write failed, RAM copy stays authoritative: {}",
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

    /// Current usage snapshot for status reports: (requests_used, request_budget).
    pub fn usage(&self) -> (i64, i64) {
        self.refresh();
        (
            self.requests_used.load(Ordering::SeqCst),
            Self::req_budget(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both tests mutate OPENROUTER_DAILY_REQ_BUDGET; serialize them so they
    // cannot race on the shared process env.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn reserve_respects_daily_cap() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("OPENROUTER_DAILY_REQ_BUDGET", "300");
        let b = OpenRouterBudget::instance();
        // Force a fresh day so the test starts from zero counters.
        b.day.store(0, Ordering::SeqCst);
        b.requests_used.store(0, Ordering::SeqCst);
        b.tokens_used.store(0, Ordering::SeqCst);
        b.db_failed.store(false, Ordering::SeqCst);

        // Default budget is 300/day.
        assert_eq!(OpenRouterBudget::req_budget(), 300);
        for _ in 0..300 {
            assert!(b.reserve().is_ok(), "reserve must succeed within budget");
        }
        let err = b.reserve().expect_err("301st request must be rejected");
        assert!(err.contains("daily request budget exceeded"));
    }

    #[test]
    fn budget_env_override() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("OPENROUTER_DAILY_REQ_BUDGET", "5");
        assert_eq!(OpenRouterBudget::req_budget(), 5);
        std::env::remove_var("OPENROUTER_DAILY_REQ_BUDGET");
    }
}
