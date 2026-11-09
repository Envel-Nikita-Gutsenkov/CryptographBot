use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

#[derive(Default, Debug)]
pub struct AiModelMetrics {
    pub ok_count: AtomicU64,
    pub err_count: AtomicU64,
}

pub static AI_METRICS: OnceLock<DashMap<String, AiModelMetrics>> = OnceLock::new();

pub fn get_metrics() -> &'static DashMap<String, AiModelMetrics> {
    AI_METRICS.get_or_init(DashMap::new)
}

pub fn record_success(model: &str) {
    let map = get_metrics();
    let entry = map.entry(model.to_string()).or_default();
    entry.ok_count.fetch_add(1, Ordering::Relaxed);
}

pub fn record_error(model: &str) {
    let map = get_metrics();
    let entry = map.entry(model.to_string()).or_default();
    entry.err_count.fetch_add(1, Ordering::Relaxed);
}

pub fn take_deltas() -> Vec<crate::models::MetricDelta> {
    let map = get_metrics();
    let mut deltas = Vec::new();
    for kv in map.iter() {
        let ok = kv.value().ok_count.swap(0, Ordering::Relaxed);
        let err = kv.value().err_count.swap(0, Ordering::Relaxed);
        if ok > 0 || err > 0 {
            deltas.push(crate::models::MetricDelta {
                category: "AI Model".to_string(),
                item: kv.key().clone(),
                ok,
                err,
            });
        }
    }
    deltas
}

// ── Internet search stats (per provider: Tavily / Exa / Local) ───────────────

pub static SEARCH_METRICS: OnceLock<DashMap<String, AiModelMetrics>> = OnceLock::new();

pub fn get_search_metrics() -> &'static DashMap<String, AiModelMetrics> {
    SEARCH_METRICS.get_or_init(DashMap::new)
}

pub fn record_search_success(provider: &str) {
    let entry = get_search_metrics()
        .entry(provider.to_string())
        .or_default();
    entry.ok_count.fetch_add(1, Ordering::Relaxed);
}

pub fn record_search_error(provider: &str) {
    let entry = get_search_metrics()
        .entry(provider.to_string())
        .or_default();
    entry.err_count.fetch_add(1, Ordering::Relaxed);
}

pub fn take_search_deltas() -> Vec<crate::models::MetricDelta> {
    let map = get_search_metrics();
    let mut deltas = Vec::new();
    for kv in map.iter() {
        let ok = kv.value().ok_count.swap(0, Ordering::Relaxed);
        let err = kv.value().err_count.swap(0, Ordering::Relaxed);
        if ok > 0 || err > 0 {
            deltas.push(crate::models::MetricDelta {
                category: "Search".to_string(),
                item: kv.key().clone(),
                ok,
                err,
            });
        }
    }
    deltas
}
