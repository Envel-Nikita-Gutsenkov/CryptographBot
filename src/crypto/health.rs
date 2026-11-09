#![allow(dead_code)]
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use std::collections::HashMap;
use tracing::warn;

use std::sync::atomic::{AtomicU64, Ordering};

const MAX_FAILURES: u32 = 3;
const RECOVERY_MINS: i64 = 15;
/// Rate limits and quota errors do not clear in 15 minutes, and probing them
/// anyway just burns more of the same quota.
const RATE_LIMIT_MINS: i64 = 60;

#[derive(Debug, Default)]
pub struct SourceHealth {
    pub name: &'static str,
    pub consecutive_failures: u32,
    pub total_ok: AtomicU64,
    pub total_err: AtomicU64,
    pub last_ok: Option<DateTime<Utc>>,
    pub last_err: Option<DateTime<Utc>>,
    /// Rolling average % deviation from consensus price.
    pub avg_deviation_pct: f64,
    pub is_healthy: bool,
    /// Cooldown override for the current failure streak (0 = default).
    cooldown_mins: i64,
}

impl SourceHealth {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            is_healthy: true,
            ..Default::default()
        }
    }

    pub fn record_ok(&mut self, deviation_pct: f64) {
        let was_unhealthy = !self.is_healthy;
        let prev_failures = self.consecutive_failures;
        self.consecutive_failures = 0;
        self.total_ok.fetch_add(1, Ordering::Relaxed);
        self.last_ok = Some(Utc::now());
        // EMA with α=0.3
        self.avg_deviation_pct = 0.7 * self.avg_deviation_pct + 0.3 * deviation_pct;
        self.is_healthy = true;
        self.cooldown_mins = 0;
        if was_unhealthy {
            warn!(
                "Source {} recovered after {} consecutive failures",
                self.name, prev_failures
            );
        }
    }

    /// Track how far a source's price sits from consensus without penalising
    /// its health. A DEX aggregator structurally quotes a different pool than a
    /// CEX, so a deviation is not an outage — logging it as a failure used to
    /// ban dexscreener (and spam the journal) twice an hour.
    pub fn record_deviation(&mut self, deviation_pct: f64) {
        self.avg_deviation_pct = 0.7 * self.avg_deviation_pct + 0.3 * deviation_pct;
    }

    pub fn record_err(&mut self, reason: &str) {
        let was_healthy = self.is_healthy;
        let lower = reason.to_lowercase();
        if lower.contains("rate limit")
            || lower.contains("429")
            || lower.contains("quota")
            || lower.contains("too many requests")
        {
            self.cooldown_mins = RATE_LIMIT_MINS;
        }
        self.consecutive_failures += 1;
        self.total_err.fetch_add(1, Ordering::Relaxed);
        self.last_err = Some(Utc::now());
        if self.consecutive_failures >= MAX_FAILURES {
            self.is_healthy = false;
            // Log once per transition, not once per failure: 3-6 providers fail
            // together on a network blip and each attempt used to print a line.
            if was_healthy {
                warn!(
                    "Source {} marked UNHEALTHY after {} failures: {}",
                    self.name, self.consecutive_failures, reason
                );
            }
        }
    }

    /// Allow re-probe after recovery window.
    pub fn should_probe(&self) -> bool {
        if self.is_healthy {
            return true;
        }
        let mins = if self.cooldown_mins > 0 {
            self.cooldown_mins
        } else {
            RECOVERY_MINS
        };
        match self.last_err {
            None => true,
            Some(t) => (Utc::now() - t).num_minutes() >= mins,
        }
    }

    pub fn status_emoji(&self) -> &'static str {
        if self.is_healthy {
            "🟢"
        } else {
            "🔴"
        }
    }
}

pub struct HealthTracker {
    sources: RwLock<HashMap<&'static str, SourceHealth>>,
}

impl HealthTracker {
    pub fn new(source_names: &[&'static str]) -> Self {
        let mut map = HashMap::new();
        for &name in source_names {
            map.insert(name, SourceHealth::new(name));
        }
        Self {
            sources: RwLock::new(map),
        }
    }

    pub fn record_ok(&self, source: &'static str, deviation_pct: f64) {
        self.sources
            .write()
            .entry(source)
            .or_insert_with(|| SourceHealth::new(source))
            .record_ok(deviation_pct);
    }

    pub fn record_err(&self, source: &'static str, reason: &str) {
        self.sources
            .write()
            .entry(source)
            .or_insert_with(|| SourceHealth::new(source))
            .record_err(reason);
    }

    pub fn record_deviation(&self, source: &'static str, deviation_pct: f64) {
        self.sources
            .write()
            .entry(source)
            .or_insert_with(|| SourceHealth::new(source))
            .record_deviation(deviation_pct);
    }

    pub fn is_healthy(&self, source: &'static str) -> bool {
        self.sources
            .read()
            .get(source)
            .map(|s| s.is_healthy)
            .unwrap_or(true)
    }

    pub fn should_probe(&self, source: &'static str) -> bool {
        self.sources
            .read()
            .get(source)
            .map(|s| s.should_probe())
            .unwrap_or(true)
    }

    pub fn take_deltas(&self) -> Vec<crate::models::MetricDelta> {
        let mut deltas = Vec::new();
        for s in self.sources.read().values() {
            let ok = s.total_ok.swap(0, Ordering::Relaxed);
            let err = s.total_err.swap(0, Ordering::Relaxed);
            if ok > 0 || err > 0 {
                deltas.push(crate::models::MetricDelta {
                    category: "Data Source".to_string(),
                    item: s.name.to_string(),
                    ok,
                    err,
                });
            }
        }
        deltas
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_failures_cooldown_longer_than_plain_ones() {
        let mut s = SourceHealth::new("probe");
        s.record_err("network timeout");
        assert_eq!(s.cooldown_mins, 0, "plain failures keep the 15m window");

        let mut r = SourceHealth::new("probe");
        r.record_err("HTTP 429: you are over your rate limit");
        assert_eq!(r.cooldown_mins, RATE_LIMIT_MINS);
        // 20 minutes later the plain source may be re-probed, the rate-limited
        // one must not be.
        s.last_err = Some(Utc::now() - chrono::Duration::minutes(20));
        s.is_healthy = false;
        r.last_err = Some(Utc::now() - chrono::Duration::minutes(20));
        r.is_healthy = false;
        assert!(s.should_probe());
        assert!(!r.should_probe());

        // A success clears the override.
        r.record_ok(0.0);
        assert_eq!(r.cooldown_mins, 0);
    }
}
