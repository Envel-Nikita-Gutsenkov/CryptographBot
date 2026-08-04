#![allow(dead_code)]
use std::collections::HashMap;
use parking_lot::RwLock;
use chrono::{DateTime, Utc};
use tracing::warn;

use std::sync::atomic::{AtomicU64, Ordering};

const MAX_FAILURES: u32 = 3;
const RECOVERY_MINS: i64 = 15;

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
}

impl SourceHealth {
    fn new(name: &'static str) -> Self {
        Self { name, is_healthy: true, ..Default::default() }
    }

    pub fn record_ok(&mut self, deviation_pct: f64) {
        self.consecutive_failures = 0;
        self.total_ok.fetch_add(1, Ordering::Relaxed);
        self.last_ok = Some(Utc::now());
        // EMA with α=0.3
        self.avg_deviation_pct = 0.7 * self.avg_deviation_pct + 0.3 * deviation_pct;
        self.is_healthy = true;
    }

    pub fn record_err(&mut self, reason: &str) {
        self.consecutive_failures += 1;
        self.total_err.fetch_add(1, Ordering::Relaxed);
        self.last_err = Some(Utc::now());
        if self.consecutive_failures >= MAX_FAILURES {
            warn!("Source {} marked UNHEALTHY after {} failures: {}", self.name, self.consecutive_failures, reason);
            self.is_healthy = false;
        }
    }

    /// Allow re-probe after recovery window.
    pub fn should_probe(&self) -> bool {
        if self.is_healthy { return true; }
        match self.last_err {
            None => true,
            Some(t) => (Utc::now() - t).num_minutes() >= RECOVERY_MINS,
        }
    }

    pub fn status_emoji(&self) -> &'static str {
        if self.is_healthy { "🟢" } else { "🔴" }
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
        Self { sources: RwLock::new(map) }
    }

    pub fn record_ok(&self, source: &'static str, deviation_pct: f64) {
        self.sources.write().entry(source).or_insert_with(|| SourceHealth::new(source)).record_ok(deviation_pct);
    }

    pub fn record_err(&self, source: &'static str, reason: &str) {
        self.sources.write().entry(source).or_insert_with(|| SourceHealth::new(source)).record_err(reason);
    }

    pub fn is_healthy(&self, source: &'static str) -> bool {
        self.sources.read().get(source).map(|s| s.is_healthy).unwrap_or(true)
    }

    pub fn should_probe(&self, source: &'static str) -> bool {
        self.sources.read().get(source).map(|s| s.should_probe()).unwrap_or(true)
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
