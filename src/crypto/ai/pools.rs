use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long the last successful model stays pinned to the front of every pool.
/// Long enough to ride out an entire burst (and survive the day's rate-limit
/// cycle without re-brute-forcing), short enough to notice when it goes stale.
const SUCCESS_MEMORY: Duration = Duration::from_secs(6 * 3600);

static LAST_SUCCESSFUL_MODEL: Mutex<Option<(String, Instant)>> = Mutex::new(None);
pub static CIRCUIT_BREAKER: std::sync::OnceLock<DashMap<String, Instant>> =
    std::sync::OnceLock::new();

/// Success memory persisted next to the SQLite DB so it survives restarts.
/// "model_memory.json" in the same directory as SQLITE_DB (default "crypto.db").
const MEMORY_FILE: &str = "model_memory.json";

#[derive(Serialize, Deserialize)]
struct MemoryFile {
    model: String,
    unix_secs: u64,
}

fn memory_path() -> std::path::PathBuf {
    let db = std::env::var("SQLITE_DB").unwrap_or_else(|_| "crypto.db".to_string());
    let p = std::path::Path::new(&db);
    match p.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(MEMORY_FILE),
        _ => std::path::PathBuf::from(MEMORY_FILE),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn record_successful_model(alias: &str) {
    if let Ok(mut m) = LAST_SUCCESSFUL_MODEL.lock() {
        *m = Some((alias.to_string(), Instant::now()));
    }
    // Persist best-effort; a failed write must never break the pipeline.
    if let Ok(js) = serde_json::to_string(&MemoryFile {
        model: alias.to_string(),
        unix_secs: now_unix(),
    }) {
        let path = memory_path();
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, js).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Load persisted success memory on first use (once per process).
fn load_persisted_memory() {
    let Ok(contents) = std::fs::read_to_string(memory_path()) else {
        return;
    };
    let Ok(mf) = serde_json::from_str::<MemoryFile>(&contents) else {
        return;
    };
    let now = now_unix();
    if mf.unix_secs == 0 || now.saturating_sub(mf.unix_secs) > SUCCESS_MEMORY.as_secs() {
        return;
    }
    if let Ok(mut m) = LAST_SUCCESSFUL_MODEL.lock() {
        let age = Duration::from_secs(now.saturating_sub(mf.unix_secs));
        if m.is_none() {
            *m = Some((
                mf.model,
                Instant::now().checked_sub(age).unwrap_or_else(Instant::now),
            ));
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    pub display: String,
    pub alias: String,
    pub provider: String,
    pub roles: Vec<String>,
    pub priority: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsFile {
    pub models: Vec<ModelConfig>,
}

/// Providers (deduplicated, in models.json order) that serve a given alias.
/// An alias can map to several provider entries (e.g. gpt-oss-120b is served
/// by both Groq and Cerebras); the footer shows all of them, joined with "/".
pub fn providers_for_alias(alias: &str) -> Vec<String> {
    let file_content = std::fs::read_to_string("models.json").unwrap_or_default();
    let models_file: ModelsFile =
        serde_json::from_str(&file_content).unwrap_or_else(|_| ModelsFile { models: vec![] });
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in &models_file.models {
        if m.alias == alias && seen.insert(m.provider.clone()) {
            out.push(m.provider.clone());
        }
    }
    out
}

pub fn ban_provider_model(provider: &crate::crypto::ai::providers::Provider, model_id: &str) {
    let key = format!("{:?}:{}", provider, model_id);
    CIRCUIT_BREAKER
        .get_or_init(DashMap::new)
        .insert(key, Instant::now());
}

pub fn is_banned(provider: &crate::crypto::ai::providers::Provider, model_id: &str) -> bool {
    let key = format!("{:?}:{}", provider, model_id);
    if let Some(time) = CIRCUIT_BREAKER.get_or_init(DashMap::new).get(&key) {
        if time.value().elapsed() < Duration::from_secs(60) {
            return true;
        }
    }
    false
}

pub fn get_available_models(role: Option<&str>) -> Vec<String> {
    let file_content = std::fs::read_to_string("models.json").unwrap_or_default();
    let mut models_file: ModelsFile =
        serde_json::from_str(&file_content).unwrap_or_else(|_| ModelsFile { models: vec![] });

    // Sort by priority
    models_file.models.sort_by_key(|m| m.priority);

    // Tier hierarchy for graceful degradation: if all strong models are dead/skipped,
    // the pipeline falls back to weaker-tier models instead of hard-failing.
    fn role_tier(r: &str) -> usize {
        match r.to_lowercase().as_str() {
            "main" | "critic" => 0,
            "news" | "macro" => 1,
            "watcher" => 2,
            _ => 3,
        }
    }
    fn key_available(provider_lower: &str) -> bool {
        match provider_lower {
            "gemini" => !std::env::var("GEMINI_API_KEY")
                .unwrap_or_default()
                .is_empty(),
            "openrouter" => !std::env::var("OPENROUTER_API_KEY")
                .unwrap_or_default()
                .is_empty(),
            "groq" => !std::env::var("GROQ_API_KEY").unwrap_or_default().is_empty(),
            "mistral" => !std::env::var("MISTRAL_API_KEY")
                .unwrap_or_default()
                .is_empty(),
            "nvidia" => !std::env::var("NVIDIA_API_KEY")
                .unwrap_or_default()
                .is_empty(),
            "cohere" => !std::env::var("COHERE_API_KEY")
                .unwrap_or_default()
                .is_empty(),
            "cerebras" => !std::env::var("CEREBRAS_API_KEY")
                .unwrap_or_default()
                .is_empty(),
            "huggingface" | "hf" => !std::env::var("HF_API_KEY").unwrap_or_default().is_empty(),
            "cloudflare" | "cf" => !std::env::var("CLOUDFLARE_API_TOKEN")
                .unwrap_or_default()
                .is_empty(),
            "deepseek" | "ds" => !std::env::var("DEEPSEEK_API_KEY")
                .unwrap_or_default()
                .is_empty(),
            _ => false,
        }
    }

    let mut available: Vec<String> = Vec::new();
    // Tier 1: exact role match (models built for this task)
    for m in &models_file.models {
        if let Some(r) = role {
            if !m
                .roles
                .iter()
                .any(|mr| mr.to_lowercase() == r.to_lowercase())
            {
                continue;
            }
        }
        if key_available(&m.provider.to_lowercase()) {
            available.push(m.alias.clone());
        }
    }

    // Tier 2: graceful degradation — append weaker-tier models not already included.
    if let Some(r) = role {
        let req_tier = role_tier(r);
        for m in &models_file.models {
            if available.contains(&m.alias) || !key_available(&m.provider.to_lowercase()) {
                continue;
            }
            let tier = m.roles.iter().map(|mr| role_tier(mr)).min().unwrap_or(3);
            if tier > req_tier {
                available.push(m.alias.clone());
            }
        }
    }

    if let Ok(g) = LAST_SUCCESSFUL_MODEL.lock() {
        if g.is_none() {
            drop(g);
            load_persisted_memory();
        }
    }
    if let Ok(g) = LAST_SUCCESSFUL_MODEL.lock() {
        if let Some((model, time)) = &*g {
            if Instant::now().duration_since(*time) < SUCCESS_MEMORY {
                if let Some(idx) = available.iter().position(|m| m == model.as_str()) {
                    let p = available.remove(idx);
                    available.insert(0, p);
                }
            }
        }
    }

    available
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_for_single_provider_alias() {
        // command-a-plus exists only under Cohere.
        let p = providers_for_alias("command-a-plus");
        assert_eq!(p, vec!["Cohere"]);
    }

    #[test]
    fn providers_for_ambiguous_alias_dedup() {
        // gpt-oss-120b is served by both Groq and Cerebras.
        let p = providers_for_alias("gpt-oss-120b");
        assert!(p.contains(&"Groq".to_string()));
        assert!(p.contains(&"Cerebras".to_string()));
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn providers_for_unknown_alias_empty() {
        assert!(providers_for_alias("no-such-model").is_empty());
    }
}
