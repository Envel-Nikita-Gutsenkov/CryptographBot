use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long the last successful model stays pinned to the front of every pool.
/// Long enough to ride out an entire burst (and survive the day's rate-limit
/// cycle without re-brute-forcing), short enough to notice when it goes stale.
const SUCCESS_MEMORY: Duration = Duration::from_secs(6 * 3600);

static LAST_SUCCESSFUL_MODEL: Mutex<Option<(String, Instant)>> = Mutex::new(None);
pub static CIRCUIT_BREAKER: std::sync::OnceLock<DashMap<String, Instant>> =
    std::sync::OnceLock::new();
/// Providers whose *key-wide* free quota is exhausted (Cloudflare neurons,
/// Groq TPD, Gemini daily). Unlike the per-model circuit breaker this is keyed
/// by provider name: once "daily free allocation" is gone, every model on that
/// provider is gone, and hammering it every 60 s only burns the /ask latency
/// budget (and floods the logs). Entries expire at the quota reset time.
pub static PROVIDER_EXHAUSTED: std::sync::OnceLock<DashMap<String, Instant>> =
    std::sync::OnceLock::new();

/// Per-alias outcome counters for the current UTC day (in-memory only).
/// A model that fails every single call it gets today is silently broken for
/// today's prompts (typically a TPM gate smaller than our news prompt) and must
/// stop occupying a selection slot — otherwise every /ask pays its latency.
/// The counter resets at UTC midnight, so a transient bad day is self-healing.
static TODAY_OUTCOMES: std::sync::OnceLock<DashMap<String, (String, u32, u32)>> =
    std::sync::OnceLock::new();

/// Minimum calls before a 100%-failure model is considered dead for the day.
const DEAD_MIN_CALLS: u32 = 8;

fn today_key() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// Record one dispatch outcome for `alias` (called by the provider layer).
pub fn record_model_outcome(alias: &str, ok: bool) {
    let today = today_key();
    let map = TODAY_OUTCOMES.get_or_init(DashMap::new);
    let mut e = map
        .entry(alias.to_string())
        .or_insert_with(|| (today.clone(), 0, 0));
    if e.0 != today {
        *e = (today, 0, 0);
    }
    e.1 = e.1.saturating_add(1);
    if !ok {
        e.2 = e.2.saturating_add(1);
    }
}

/// True when `alias` has been called at least [`DEAD_MIN_CALLS`] times today and
/// has failed every single time.
pub fn model_is_dead_today(alias: &str) -> bool {
    let Some(map) = TODAY_OUTCOMES.get() else {
        return false;
    };
    let today = today_key();
    match map.get(alias) {
        Some(e) => e.0 == today && e.1 >= DEAD_MIN_CALLS && e.2 == e.1,
        None => false,
    }
}

/// Aliases currently considered dead for today, for `/aihealth`.
pub fn dead_models_today() -> Vec<String> {
    let Some(map) = TODAY_OUTCOMES.get() else {
        return Vec::new();
    };
    let today = today_key();
    let mut out: Vec<String> = map
        .iter()
        .filter(|e| {
            e.value().0 == today && e.value().1 >= DEAD_MIN_CALLS && e.value().2 == e.value().1
        })
        .map(|e| e.key().clone())
        .collect();
    out.sort();
    out
}

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

/// Generated pool file written by the OpenRouter ranker, merged with models.json.
const RANK_FILE: &str = "models.openrouter.json";

/// Load models.json + models.openrouter.json (generated by the model ranker)
/// into one list. The ranker file never clobbers hand-edited entries: for the
/// same (alias, provider) pair the ranked entry wins, otherwise models.json
/// stays authoritative.
pub fn load_all_models() -> Vec<ModelConfig> {
    let mut models = Vec::new();
    let file_content = std::fs::read_to_string("models.json").unwrap_or_default();
    let models_file: ModelsFile =
        serde_json::from_str(&file_content).unwrap_or_else(|_| ModelsFile { models: vec![] });
    models.extend(models_file.models);

    let rank_content = std::fs::read_to_string(RANK_FILE).unwrap_or_default();
    let rank_file: ModelsFile =
        serde_json::from_str(&rank_content).unwrap_or_else(|_| ModelsFile { models: vec![] });

    // Dedupe: ranked entries replace models.json entries with the same (alias, provider).
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for m in &rank_file.models {
        seen.insert((m.alias.clone(), m.provider.clone()));
    }
    models.retain(|m| !seen.contains(&(m.alias.clone(), m.provider.clone())));
    models.extend(rank_file.models);
    models
}

/// Providers (deduplicated, in models.json order) that serve a given alias.
/// An alias can map to several provider entries (e.g. gpt-oss-120b is served
/// by both Groq and Cerebras); the footer shows all of them, joined with "/".
pub fn providers_for_alias(alias: &str) -> Vec<String> {
    let models = load_all_models();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in &models {
        if m.alias == alias && seen.insert(m.provider.clone()) {
            out.push(m.provider.clone());
        }
    }
    out
}

/// Ban a provider/model for `duration_secs`. Transient failures (timeout, 5xx,
/// 429) get a short 60s ban; permanent ones (auth/4xx) get 1h so dead models
/// stop being hammered by call sites that don't pass the DB handle.
pub fn ban_provider_model(
    provider: &crate::crypto::ai::providers::Provider,
    model_id: &str,
    duration_secs: u64,
) {
    let key = format!("{:?}:{}", provider, model_id);
    CIRCUIT_BREAKER
        .get_or_init(DashMap::new)
        .insert(key, Instant::now() + Duration::from_secs(duration_secs));
}

/// Drop any bench for this provider/model after a live probe proved it answers.
/// Needed because the in-memory breaker is the only thing that can keep a
/// perfectly healthy model out of the pool, and it has no natural expiry hint
/// beyond the coarse durations set at failure time.
pub fn clear_provider_model_ban(provider: &crate::crypto::ai::providers::Provider, model_id: &str) {
    if let Some(map) = CIRCUIT_BREAKER.get() {
        map.remove(&format!("{:?}:{}", provider, model_id));
    }
    if let Some(map) = PROVIDER_EXHAUSTED.get() {
        map.remove(&format!("{:?}", provider));
    }
}

/// Seconds until the next UTC midnight (when free daily quotas reset).
pub fn secs_until_utc_midnight() -> u64 {
    use chrono::{Timelike, Utc};
    let now = Utc::now();
    let secs_today = now.num_seconds_from_midnight() as u64;
    86_400u64.saturating_sub(secs_today)
}

/// Seconds until 00:00 UTC on the 1st of next month (when monthly trial caps
/// reset). Used for keys that report a month-wide allowance.
pub fn secs_until_next_month_start() -> u64 {
    use chrono::{Datelike, TimeZone, Utc};
    let now = Utc::now();
    let (year, month) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    let next = Utc
        .with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .unwrap_or(now);
    (next - now).num_seconds().max(60) as u64
}

/// Parse a retry hint out of an error body: `try again in 53m58.272s`,
/// `retry in 30s`, `"retryDelay": "22s"`. Returns seconds.
fn parse_retry_after(err_lower: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut found = false;
    // Scan for `<number>[hms]` groups after a retry marker.
    let marker = ["try again in", "retry in", "retrydelay", "retry after"]
        .iter()
        .filter_map(|m| err_lower.find(m).map(|i| (i, m.len())))
        .min_by_key(|(i, _)| *i);
    let Some((pos, mlen)) = marker else {
        return None;
    };
    let tail = &err_lower[pos + mlen..];
    let mut num = String::new();
    for ch in tail.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num.push(ch);
        } else if matches!(ch, 'h' | 'm' | 's') {
            let Ok(v) = num.parse::<f64>() else {
                return None;
            };
            total = total.saturating_add(match ch {
                'h' => (v * 3600.0) as u64,
                'm' => (v * 60.0) as u64,
                _ => v as u64,
            });
            found = true;
            num.clear();
        } else if found && !ch.is_whitespace() && ch != ',' && ch != '\\' && ch != '"' {
            break;
        } else if !found && !ch.is_whitespace() && ch != ':' && ch != '"' {
            // Ran into some other text before any unit was parsed.
            if !num.is_empty() {
                break;
            }
        }
    }
    found.then_some(total.max(1))
}

/// How long a provider/model should be benched for this error, if it is a
/// quota exhaustion rather than a short burst limit.
///
/// Free tiers report several shapes of the same thing:
/// * Cloudflare — "used up your daily free allocation of 10,000 neurons"
/// * Groq — "tokens per day (TPD): Limit 200000 ... try again in 53m58"
/// * Gemini — "You exceeded your current quota" / RESOURCE_EXHAUSTED
///
/// A generic 429 must NOT bench a model for long: it recovers in seconds. But a
/// daily quota will not recover for hours, and retrying it is pure waste.
pub fn quota_cooldown_secs(err: &str) -> Option<u64> {
    let e = err.to_lowercase();
    let is_quota = e.contains("429")
        || e.contains("rate limit")
        || e.contains("too many requests")
        || e.contains("quota")
        || e.contains("resource_exhausted");
    if !is_quota {
        return None;
    }
    let daily = e.contains("daily free allocation")
        || e.contains("neurons")
        || e.contains("tokens per day")
        || e.contains("(tpd)")
        || e.contains("per day")
        || e.contains("perday")
        || e.contains("daily quota")
        || e.contains("daily limit");
    // Monthly trial caps (Cohere: "Trial key, which is limited to 1000 API
    // calls / month") do not come back before the 1st of the next month; a
    // 5-minute bench made the pool re-probe a dead key all day.
    let monthly = (e.contains("trial key") && e.contains("month"))
        || e.contains("api calls / month")
        || e.contains("calls per month")
        || e.contains("monthly limit");
    if let Some(secs) = parse_retry_after(&e) {
        // Trust the provider's own reset hint, with a small safety margin.
        return Some(secs.saturating_add(30).clamp(60, 86_400));
    }
    if monthly {
        return Some(secs_until_next_month_start());
    }
    if daily {
        return Some(secs_until_utc_midnight().max(120));
    }
    // A quota error without a reset hint is still not a 1-second burst: bench
    // it briefly so a single /ask does not spend 3 retries x N models on it.
    Some(300)
}

pub fn provider_exhausted_until(provider: &str) -> Option<Instant> {
    PROVIDER_EXHAUSTED
        .get_or_init(DashMap::new)
        .get(provider)
        .map(|v| *v.value())
        .filter(|until| Instant::now() < *until)
}

/// Number of user-facing AI requests currently in flight.
static INTERACTIVE_ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// RAII guard marking an interactive (user-initiated) AI request. While at
/// least one is alive, background workers back off before spending a provider
/// quota: free tiers are small enough that a volatility-triggered sweep of
/// news/watcher/macro jobs can drain every key and leave a user's /ask with
/// nothing — the exact "лимиты исчерпаны" the owner saw during a price drop.
pub struct InteractiveGuard;

impl InteractiveGuard {
    pub fn acquire() -> Self {
        INTERACTIVE_ACTIVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

impl Drop for InteractiveGuard {
    fn drop(&mut self) {
        INTERACTIVE_ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

pub fn interactive_active() -> bool {
    INTERACTIVE_ACTIVE.load(std::sync::atomic::Ordering::SeqCst) > 0
}

/// Called by background workers before an AI call. Yields while the owner is
/// waiting for an answer, but never blocks a worker forever: after `max_wait`
/// it proceeds anyway so scheduled jobs cannot starve.
pub async fn background_gate(max_wait: std::time::Duration) {
    let deadline = Instant::now() + max_wait;
    while interactive_active() && Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

pub fn is_provider_exhausted(provider: &str) -> bool {
    provider_exhausted_until(provider).is_some()
}

/// Record that a provider's key-wide quota is gone until `until`.
pub fn mark_provider_exhausted(provider: &str, until: Instant) {
    let map = PROVIDER_EXHAUSTED.get_or_init(DashMap::new);
    let keep = map
        .get(provider)
        .map(|v| (*v.value()).max(until))
        .unwrap_or(until);
    map.insert(provider.to_string(), keep);
}

/// Human-readable list of exhausted providers and when they reset, e.g.
/// `Cloudflare (до 00:00 UTC), Groq (до 23:40 UTC)`. `None` when nothing is
/// benched, so callers can keep the generic error text.
pub fn quota_report() -> Option<String> {
    let map = PROVIDER_EXHAUSTED.get_or_init(DashMap::new);
    let now = Instant::now();
    let mut rows: Vec<(String, u64)> = map
        .iter()
        .filter_map(|e| {
            let until = *e.value();
            (until > now).then(|| {
                let secs = until.saturating_duration_since(now).as_secs();
                (e.key().clone(), secs)
            })
        })
        .collect();
    if rows.is_empty() {
        return None;
    }
    rows.sort_by_key(|(_, s)| *s);
    let parts: Vec<String> = rows
        .into_iter()
        .map(|(p, secs)| {
            let reset = chrono::Utc::now() + chrono::Duration::seconds(secs as i64);
            format!("{} (до {} UTC)", p, reset.format("%H:%M"))
        })
        .collect();
    Some(parts.join(", "))
}

pub fn is_banned(provider: &crate::crypto::ai::providers::Provider, model_id: &str) -> bool {
    let key = format!("{:?}:{}", provider, model_id);
    if let Some(until) = CIRCUIT_BREAKER.get_or_init(DashMap::new).get(&key) {
        if Instant::now() < *until.value() {
            return true;
        }
    }
    false
}

/// Round-robin over providers so one provider's four models cannot fill the
/// head of the list. Relative priority inside each provider is preserved; the
/// first round takes every provider's best model, the second round their second
/// best, and so on. Without this, `news` spent its first three calls on
/// Gemini/Cloudflare while DeepSeek and Mistral sat idle.
pub(crate) fn interleave_by_provider(models: Vec<ModelConfig>) -> Vec<ModelConfig> {
    let mut buckets: Vec<(String, Vec<ModelConfig>)> = Vec::new();
    for m in models {
        match buckets.iter_mut().find(|(p, _)| *p == m.provider) {
            Some((_, b)) => b.push(m),
            None => buckets.push((m.provider.clone(), vec![m])),
        }
    }
    let mut out = Vec::new();
    let mut round = 0;
    loop {
        let mut pushed = false;
        for (_, bucket) in buckets.iter_mut() {
            if let Some(m) = bucket.get(round) {
                out.push(m.clone());
                pushed = true;
            }
        }
        if !pushed {
            break;
        }
        round += 1;
    }
    out
}

/// Whether the env carries a key for this provider (lower-case provider name).
/// A missing key is the most common reason a whole provider silently drops out
/// of the pool, so both selection and diagnostics use this single definition.
pub(crate) fn key_available(provider_lower: &str) -> bool {
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

/// One-screen snapshot of why the pool may be empty at failure time: benched
/// models, exhausted provider quotas, models dead for today, providers with no
/// key. Callers print this next to an error so "no provider answered" becomes
/// actionable instead of a blanket claim that every provider was tried.
pub fn pool_status_report() -> Vec<String> {
    let mut lines = Vec::new();
    let now = Instant::now();

    if let Some(map) = CIRCUIT_BREAKER.get() {
        let mut bench: Vec<String> = map
            .iter()
            .filter(|e| *e.value() > now)
            .map(|e| {
                let secs = e.value().saturating_duration_since(now).as_secs();
                format!("{} (~{}s)", e.key(), secs)
            })
            .collect();
        bench.sort();
        if !bench.is_empty() {
            lines.push(format!("⛔ В бане: {}", bench.join(", ")));
        }
    }

    if let Some(q) = quota_report() {
        lines.push(format!("📉 Исчерпаны: {q}"));
    }

    let dead = dead_models_today();
    if !dead.is_empty() {
        lines.push(format!("💀 Мертвы сегодня: {}", dead.join(", ")));
    }

    let models = load_all_models();
    let mut no_key: Vec<String> = models
        .iter()
        .map(|m| m.provider.clone())
        .filter(|p| !key_available(&p.to_lowercase()))
        .collect();
    no_key.sort();
    no_key.dedup();
    if !no_key.is_empty() {
        lines.push(format!("🔑 Нет ключа: {}", no_key.join(", ")));
    }

    let usable_count = models
        .iter()
        .filter(|m| {
            key_available(&m.provider.to_lowercase()) && !is_provider_exhausted(&m.provider)
        })
        .count();
    lines.push(format!(
        "✅ Кандидатов в пуле: {usable_count}/{}",
        models.len()
    ));
    lines
}

pub fn get_available_models(role: Option<&str>) -> Vec<String> {
    let mut models_file = ModelsFile {
        models: load_all_models(),
    };

    // Sort by priority
    models_file.models.sort_by_key(|m| m.priority);

    // Fast mode (CRYPTOGRAPH_FAST_MODELS=1): restrict every role to cheap
    // watcher-tier models. Used by the scenario runner (--fast) so hundreds of
    // what-if tests complete in seconds instead of minutes; the deterministic
    // strategy layer still enforces the no-FOMO / no-panic rules.
    let fast_mode = std::env::var("CRYPTOGRAPH_FAST_MODELS").unwrap_or_default() == "1";
    let is_fast_model =
        |m: &ModelConfig| m.roles.iter().any(|r| role_tier(r) == role_tier("watcher"));

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

    /// A model on a provider whose key-wide quota is gone can only fail; skip it
    /// at selection time so every worker stops logging one "failed" line per
    /// parked model per cycle (Gemini + Cloudflare carry four models between
    /// them). The reset is recorded by the provider layer in [`PROVIDER_EXHAUSTED`].
    fn usable(m: &ModelConfig) -> bool {
        key_available(&m.provider.to_lowercase()) && !is_provider_exhausted(&m.provider)
    }

    let mut primary: Vec<ModelConfig> = Vec::new();
    // Tier 1: exact role match (models built for this task)
    for m in &models_file.models {
        if fast_mode && !is_fast_model(m) {
            continue;
        }
        if let Some(r) = role {
            if !m
                .roles
                .iter()
                .any(|mr| mr.to_lowercase() == r.to_lowercase())
            {
                continue;
            }
        }
        if usable(m) && !model_is_dead_today(&m.alias) {
            primary.push(m.clone());
        }
    }

    // Tier 2: graceful degradation — append weaker-tier models not already included.
    let mut fallback: Vec<ModelConfig> = Vec::new();
    if let Some(r) = role {
        let req_tier = role_tier(r);
        for m in &models_file.models {
            if fast_mode && !is_fast_model(m) {
                continue;
            }
            if primary.iter().any(|p| p.alias == m.alias)
                || fallback.iter().any(|f| f.alias == m.alias)
                || !usable(m)
                || model_is_dead_today(&m.alias)
            {
                continue;
            }
            let tier = m.roles.iter().map(|mr| role_tier(mr)).min().unwrap_or(3);
            if tier > req_tier {
                fallback.push(m.clone());
            }
        }
    }

    let mut available: Vec<String> = interleave_by_provider(primary)
        .into_iter()
        .chain(interleave_by_provider(fallback))
        .map(|m| m.alias)
        .collect();

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
    fn quota_cooldown_uses_provider_reset_hint() {
        // Real Groq TPD body from prod logs.
        let groq = "Groq openai/gpt-oss-120b HTTP 429: {\"error\":{\"message\":\"Rate limit reached for model `openai/gpt-oss-120b` on tokens per day (TPD): Limit 200000, Used 198697, Requested 8799. Please try again in 53m58.272s.\"}}";
        let secs = quota_cooldown_secs(groq).expect("TPD must be a quota error");
        assert!(
            (3200..=3300).contains(&secs),
            "TPD cooldown should follow the reset hint, got {secs}"
        );
    }

    #[test]
    fn quota_cooldown_marks_daily_allocations() {
        // Cloudflare neurons reset at UTC midnight, not in a minute.
        let cf = "Cloudflare @cf/meta/llama-4-scout-17b-16e-instruct HTTP 429: {\"errors\":[{\"message\":\"you have used up your daily free allocation of 10,000 neurons\"}]}";
        let secs = quota_cooldown_secs(cf).expect("daily allocation must be a quota error");
        assert!(
            secs >= 120 && secs <= 86_400,
            "daily quota cooldown out of range: {secs}"
        );
    }

    #[test]
    fn quota_cooldown_short_for_plain_quota_and_none_for_5xx() {
        let gemini = "Gemini gemini-2.5-flash HTTP 429: {\"code\": 429, \"message\": \"You exceeded your current quota, please check your plan\"}";
        assert_eq!(quota_cooldown_secs(gemini), Some(300));
        assert_eq!(quota_cooldown_secs("HTTP 500 internal server error"), None);
        assert_eq!(quota_cooldown_secs("connection timed out"), None);
    }

    #[test]
    fn provider_exhaustion_expires() {
        let until = Instant::now() + Duration::from_secs(3600);
        mark_provider_exhausted("TestProvider", until);
        assert!(is_provider_exhausted("TestProvider"));
        assert!(!is_provider_exhausted("OtherProvider"));
        assert!(quota_report().unwrap().contains("TestProvider"));
        // An expired entry is not reported.
        mark_provider_exhausted("ExpiredProvider", Instant::now() - Duration::from_secs(1));
        assert!(!is_provider_exhausted("ExpiredProvider"));
        assert!(!quota_report().unwrap().contains("ExpiredProvider"));
    }

    #[test]
    fn pool_status_report_always_states_candidate_count() {
        // The error text in /ask leans on this line to explain the failure, so
        // it must always be present, even when nothing is benched or missing.
        let report = pool_status_report();
        let last = report.last().expect("report must never be empty");
        assert!(
            last.starts_with("✅ Кандидатов в пуле:"),
            "unexpected tail line: {last}"
        );
    }

    #[test]
    fn pool_status_report_names_benched_models() {
        let provider = crate::crypto::ai::providers::Provider::Gemini;
        ban_provider_model(&provider, "status-report-probe", 60);
        let report = pool_status_report().join("\n");
        assert!(
            report.contains("status-report-probe"),
            "a benched model must be visible in the report: {report}"
        );
        clear_provider_model_ban(&provider, "status-report-probe");
    }

    #[test]
    fn quota_cooldown_detects_monthly_trial_cap() {
        let err = "Cohere command-a-plus HTTP 429: You are using a Trial key, \
                   which is limited to 1000 API calls / month.";
        let secs = quota_cooldown_secs(err).expect("a monthly cap must bench the provider");
        assert!(
            secs > 86_400,
            "a month-wide cap must bench longer than any day bucket, got {secs}"
        );
    }

    #[test]
    fn providers_for_single_provider_alias() {
        // command-a-plus exists only under Cohere.
        let p = providers_for_alias("command-a-plus");
        assert_eq!(p, vec!["Cohere"]);
    }

    #[test]
    fn providers_for_alias_resolves_single_provider() {
        // gpt-oss-120b is served only by Groq after the dead-provider prune.
        let p = providers_for_alias("gpt-oss-120b");
        assert_eq!(p, vec!["Groq"]);
    }

    #[test]
    fn providers_for_unknown_alias_empty() {
        assert!(providers_for_alias("no-such-model").is_empty());
    }

    #[test]
    fn fast_mode_restricts_to_watcher_tier() {
        // With CRYPTOGRAPH_FAST_MODELS=1, every role resolves to models that
        // carry a watcher role (cheap tier) — never the slow reasoning models.
        // Set fake keys so key_available() passes (tests don't load .env).
        for k in [
            "GROQ_API_KEY",
            "DEEPSEEK_API_KEY",
            "GEMINI_API_KEY",
            "CLOUDFLARE_API_TOKEN",
            "COHERE_API_KEY",
        ] {
            std::env::set_var(k, "test-key");
        }
        std::env::set_var("CRYPTOGRAPH_FAST_MODELS", "1");
        let mains = get_available_models(Some("main"));
        let critics = get_available_models(Some("critic"));
        std::env::remove_var("CRYPTOGRAPH_FAST_MODELS");

        // The fast set must be non-empty (models.json ships watcher models).
        assert!(!mains.is_empty(), "fast main pool must not be empty");
        assert!(!critics.is_empty(), "fast critic pool must not be empty");

        // No slow reasoning model may appear in fast mode.
        for m in mains.iter().chain(critics.iter()) {
            assert!(
                !m.contains("reasoning") && !m.contains("pro"),
                "fast mode leaked slow model {m}"
            );
        }
    }

    fn mc(provider: &str, alias: &str, priority: i32) -> ModelConfig {
        ModelConfig {
            id: alias.to_string(),
            display: alias.to_string(),
            alias: alias.to_string(),
            provider: provider.to_string(),
            roles: vec!["main".to_string()],
            priority,
        }
    }

    #[test]
    fn interleave_spreads_providers() {
        let models = vec![
            mc("Gemini", "g1", 0),
            mc("Gemini", "g2", 1),
            mc("Gemini", "g3", 2),
            mc("Groq", "q1", 3),
            mc("Mistral", "m1", 4),
        ];
        let out = interleave_by_provider(models);
        let aliases: Vec<&str> = out.iter().map(|m| m.alias.as_str()).collect();
        // One model per provider first, then the leftovers in priority order.
        assert_eq!(aliases, vec!["g1", "q1", "m1", "g2", "g3"]);
    }

    #[test]
    fn dead_model_detector_trips_only_on_total_failure() {
        let alias = "__test_dead_model__";
        for _ in 0..(DEAD_MIN_CALLS - 1) {
            record_model_outcome(alias, false);
        }
        assert!(!model_is_dead_today(alias));
        record_model_outcome(alias, false);
        assert!(model_is_dead_today(alias));
        assert!(dead_models_today().contains(&alias.to_string()));
        // A single success means the model is flaky, not dead.
        record_model_outcome(alias, true);
        assert!(!model_is_dead_today(alias));
        assert!(!dead_models_today().contains(&alias.to_string()));
    }
}
