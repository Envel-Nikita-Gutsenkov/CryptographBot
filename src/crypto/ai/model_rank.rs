//! Generalized lazy model ranking across all AI providers.
//!
//! The benchmark is designed so it can **never** be the thing that burns the
//! daily budget:
//!
//! - **Small batches**: at most [`MAX_BENCH_PER_RUN`] (default 3) models per
//!   run, one role per model per run (the role rotates across runs).
//! - **Cooldown**: a model tested within [`BENCH_COOLDOWN_DAYS`] (default 30)
//!   is skipped unless its fingerprint changed or production stats show it
//!   underperforming (error rate > 30% with > 10 calls).
//! - **Fingerprint**: OpenRouter reports an `updated` timestamp per model. If
//!   it changed since the last test, the model is re-tested early; otherwise
//!   it stays in cooldown for the full month.
//! - **Budget-aware**: OpenRouter benchmark calls check the remaining daily
//!   budget before every call and stop as soon as it drops below
//!   [`MIN_OPENROUTER_RESERVE`] (default 20). Benchmarking never exhausts the
//!   300/day cap — production traffic always has priority.
//! - **429 / rate-limit**: treated as "skip, retry later", never counted as a
//!   model failure and never recorded in `model_stats`.
//!
//! Output: [`RANK_FILE`] (`models.openrouter.json`) — an overlay merged into
//! the pools by `pools::load_all_models()`. It only manages **OpenRouter free
//! models**; `models.json` stays the hand-edited source of truth for the other
//! providers. The ranker still *benchmarks and tracks stats* for every
//! provider (same machinery), and the CLI/worker report shows per-provider
//! recommendations — but it never silently overwrites hand-curated config.

use crate::crypto::ai::pools::{ModelConfig, ModelsFile};
use crate::crypto::ai::providers::{call_single_model, ModelInfo, Provider};
use crate::db::sqlite::SqliteStore;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Ranked overlay file merged into the pools by `pools::load_all_models()`.
pub const RANK_FILE: &str = "models.openrouter.json";

/// A model tested within this many days is skipped (unless changed/underperforming).
pub const BENCH_COOLDOWN_DAYS: i64 = 30;
/// Maximum number of models benchmarked in a single run.
pub const MAX_BENCH_PER_RUN: usize = 3;
/// OpenRouter benchmark calls stop when fewer than this many requests remain.
pub const MIN_OPENROUTER_RESERVE: i64 = 20;
/// Production stats window used for underperformance detection / assignment.
pub const STATS_WINDOW_DAYS: u32 = 14;
/// Error-rate threshold (with enough calls) that forces an early re-test.
pub const ERROR_RATE_DEMOTE: f64 = 0.30;
/// Minimum production calls before error-rate demotion applies.
pub const ERROR_RATE_MIN_CALLS: i64 = 10;
/// Judge scores below this mean the judge is unreliable → no assignment this run.
pub const JUDGE_MIN_BASELINE: f64 = 4.0;

const ROLES: [&str; 4] = ["main", "critic", "news", "watcher"];

/// Role → minimum judge quality (0-10) required for assignment.
fn role_threshold(role: &str) -> f64 {
    match role {
        "main" | "critic" => 6.0,
        "news" => 5.0,
        _ => 4.0, // watcher
    }
}

#[derive(Debug, Default)]
pub struct DiscoverReport {
    pub total: usize,
    pub new: usize,
}

#[derive(Debug)]
pub struct BenchResult {
    pub provider: String,
    pub model_id: String,
    pub role: String,
    /// Why this model was picked for this run (new/changed/cooldown/underperforming).
    pub reason: &'static str,
    pub ok: bool,
    pub quality: f64,
    pub latency_ms: i64,
    pub error: Option<String>,
    /// True when the call was skipped due to a rate limit (not a model failure).
    pub skipped_rate_limit: bool,
}

#[derive(Debug, Default)]
pub struct RankReport {
    pub tested: Vec<BenchResult>,
    pub skipped_tested: usize,
    pub assigned: Vec<ModelConfig>,
    pub judge_baseline: f64,
    pub judge_reliable: bool,
    pub budget_remaining: i64,
}

// ── Discovery ────────────────────────────────────────────────────────────────

/// Upsert models.json entries (all providers) + OpenRouter free models into
/// the registry. Returns (total upserted, newly seen).
pub async fn discover(
    client: &reqwest::Client,
    db: &Arc<SqliteStore>,
) -> anyhow::Result<DiscoverReport> {
    let existing: std::collections::HashSet<(String, String)> = db
        .get_model_registry()?
        .into_iter()
        .map(|r| (r.0, r.1))
        .collect();

    let mut total = 0;
    let mut new = 0;

    // 1. Hand-configured entries from models.json (fingerprint unknown → "").
    for m in crate::crypto::ai::pools::load_all_models() {
        let provider = provider_name(&m.provider);
        let key = (provider.clone(), m.id.clone());
        if !existing.contains(&key) {
            new += 1;
        }
        db.upsert_model_registry(&provider, &m.id, &m.alias, &m.display, 0, "")?;
        total += 1;
    }

    // 2. OpenRouter free models (only if the key is configured).
    for (id, name, ctx, updated) in fetch_free_models(client).await? {
        let key = ("OpenRouter".to_string(), id.clone());
        if !existing.contains(&key) {
            new += 1;
        }
        db.upsert_model_registry("OpenRouter", &id, &id, &name, ctx, &updated)?;
        total += 1;
    }

    Ok(DiscoverReport { total, new })
}

/// Fetch free models from the OpenRouter API: (id, name, context_length, updated).
/// The models-list endpoint does not consume the 300/day chat-completion budget.
async fn fetch_free_models(
    client: &reqwest::Client,
) -> anyhow::Result<Vec<(String, String, i64, String)>> {
    let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Ok(Vec::new());
    }
    let resp = client
        .get("https://openrouter.ai/api/v1/models")
        .header("Authorization", format!("Bearer {key}"))
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "OpenRouter models list HTTP {status}: {}",
            crate::crypto::ai::providers::clean_model_output(&body)
                .chars()
                .take(200)
                .collect::<String>()
        ));
    }
    let json: serde_json::Value = resp.json().await?;
    let mut out = Vec::new();
    for m in json["data"].as_array().cloned().unwrap_or_default() {
        let pricing = &m["pricing"];
        let is_free =
            pricing["prompt"].as_str() == Some("0") && pricing["completion"].as_str() == Some("0");
        if !is_free {
            continue;
        }
        let id = m["id"].as_str().unwrap_or("").to_string();
        if id.is_empty() {
            continue;
        }
        let name = m["name"].as_str().unwrap_or(&id).to_string();
        let ctx = m["context_length"].as_i64().unwrap_or(0);
        let updated = m["updated"]
            .as_i64()
            .map(|u| u.to_string())
            .unwrap_or_default();
        out.push((id, name, ctx, updated));
    }
    Ok(out)
}

// ── Lazy candidate selection ─────────────────────────────────────────────────

struct Candidate {
    provider: String,
    model_id: String,
    alias: String,
    display: String,
    role: String,
    fingerprint: String,
    reason: &'static str,
}

/// Pick the lazy batch: new models, changed models, cooldown-expired models,
/// and production underperformers — capped at [`MAX_BENCH_PER_RUN`].
fn select_candidates(
    registry: &[(
        String,
        String,
        String,
        String,
        i64,
        String,
        Option<String>,
        String,
        i64,
    )],
    stats: &[(String, String, i64, i64, i64, f64)],
    openrouter_remaining: i64,
) -> Vec<Candidate> {
    let today = chrono::Utc::now().date_naive();
    let mut candidates = Vec::new();

    for (
        provider,
        model_id,
        alias,
        display,
        _ctx,
        fingerprint,
        last_tested,
        tested_fp,
        bench_count,
    ) in registry
    {
        // Never let benchmarking threaten the OpenRouter daily cap.
        if provider == "OpenRouter" && openrouter_remaining < MIN_OPENROUTER_RESERVE {
            continue;
        }
        let role = ROLES[(*bench_count as usize) % ROLES.len()];

        // Brand-new model → test it.
        if *bench_count == 0 {
            candidates.push(Candidate {
                provider: provider.clone(),
                model_id: model_id.clone(),
                alias: alias.clone(),
                display: display.clone(),
                role: role.to_string(),
                fingerprint: fingerprint.clone(),
                reason: "new",
            });
            continue;
        }

        // Fingerprint changed since the last test → re-test early.
        if !fingerprint.is_empty() && !tested_fp.is_empty() && fingerprint != tested_fp {
            candidates.push(Candidate {
                provider: provider.clone(),
                model_id: model_id.clone(),
                alias: alias.clone(),
                display: display.clone(),
                role: role.to_string(),
                fingerprint: fingerprint.clone(),
                reason: "changed",
            });
            continue;
        }

        // Cooldown expired → re-test (the "remember for a month" rule).
        if let Some(tested) = last_tested {
            if let Ok(ts) = chrono::NaiveDateTime::parse_from_str(tested, "%Y-%m-%d %H:%M:%S") {
                if (today - ts.date()).num_days() >= BENCH_COOLDOWN_DAYS {
                    candidates.push(Candidate {
                        provider: provider.clone(),
                        model_id: model_id.clone(),
                        alias: alias.clone(),
                        display: display.clone(),
                        role: role.to_string(),
                        fingerprint: fingerprint.clone(),
                        reason: "cooldown-expired",
                    });
                    continue;
                }
            }
        }

        // Underperforming in production → re-test regardless of cooldown.
        if let Some((calls, errors, _, _)) = stats_for(provider, model_id, stats) {
            if calls > ERROR_RATE_MIN_CALLS && errors as f64 / calls as f64 > ERROR_RATE_DEMOTE {
                candidates.push(Candidate {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                    alias: alias.clone(),
                    display: display.clone(),
                    role: role.to_string(),
                    fingerprint: fingerprint.clone(),
                    reason: "underperforming",
                });
            }
        }
    }

    candidates.truncate(MAX_BENCH_PER_RUN);
    candidates
}

fn stats_for<'a>(
    provider: &str,
    model_id: &str,
    stats: &'a [(String, String, i64, i64, i64, f64)],
) -> Option<(i64, i64, i64, f64)> {
    stats
        .iter()
        .find(|(p, m, _, _, _, _)| p == provider && m == model_id)
        .map(|(_, _, calls, errors, lat, q)| (*calls, *errors, *lat, *q))
}

// ── Benchmarking ─────────────────────────────────────────────────────────────

/// Run one role benchmark for one model. Records results into `model_stats`
/// and marks the model tested (unless the call was rate-limited).
async fn bench_one(
    client: &reqwest::Client,
    db: &Arc<SqliteStore>,
    cand: &Candidate,
) -> BenchResult {
    let prompt = sample_prompt(&cand.role).to_string();
    let info = ModelInfo {
        id: cand.model_id.clone(),
        display: cand.display.clone(),
        provider: Provider::from_str(&cand.provider).unwrap_or(Provider::OpenRouter),
        alias: cand.alias.clone(),
    };
    let gemini_key = std::env::var("GEMINI_API_KEY").unwrap_or_default();

    let start = Instant::now();
    let result = call_single_model(client, &gemini_key, &info, &prompt, Some(db.clone())).await;
    let latency_ms = start.elapsed().as_millis() as i64;

    match result {
        Ok(response) => {
            let quality = judge_response(client, db, &cand.role, &prompt, &response).await;
            let _ =
                db.record_model_stats(&cand.provider, &cand.model_id, latency_ms, true, quality);
            let _ = db.mark_model_tested(&cand.provider, &cand.model_id, &cand.fingerprint);
            BenchResult {
                provider: cand.provider.clone(),
                model_id: cand.model_id.clone(),
                role: cand.role.clone(),
                reason: cand.reason,
                ok: true,
                quality,
                latency_ms,
                error: None,
                skipped_rate_limit: false,
            }
        }
        Err(e) => {
            if is_rate_limit(&e) {
                // Not a model failure — leave it untested so it retries later.
                BenchResult {
                    provider: cand.provider.clone(),
                    model_id: cand.model_id.clone(),
                    role: cand.role.clone(),
                    reason: cand.reason,
                    ok: false,
                    quality: 0.0,
                    latency_ms,
                    error: Some(e.to_string()),
                    skipped_rate_limit: true,
                }
            } else {
                let _ =
                    db.record_model_stats(&cand.provider, &cand.model_id, latency_ms, false, 0.0);
                let _ = db.mark_model_tested(&cand.provider, &cand.model_id, &cand.fingerprint);
                BenchResult {
                    provider: cand.provider.clone(),
                    model_id: cand.model_id.clone(),
                    role: cand.role.clone(),
                    reason: cand.reason,
                    ok: false,
                    quality: 0.0,
                    latency_ms,
                    error: Some(e.to_string()),
                    skipped_rate_limit: false,
                }
            }
        }
    }
}

fn is_rate_limit(err: &anyhow::Error) -> bool {
    let s = err.to_string().to_lowercase();
    s.contains("429")
        || s.contains("rate limit")
        || s.contains("too many requests")
        || s.contains("quota")
        || s.contains("overloaded")
        || s.contains("try again later")
}

// ── Judge ────────────────────────────────────────────────────────────────────

/// Ask the judge model to score a benchmark response (0-10).
async fn judge_response(
    client: &reqwest::Client,
    db: &Arc<SqliteStore>,
    role: &str,
    sample: &str,
    response: &str,
) -> f64 {
    let prompt = judge_prompt(role, sample, response);
    match call_judge(client, &prompt, Some(db.clone())).await {
        Ok(text) => parse_judge_score(&text).unwrap_or(5.0),
        // Judge unavailable → neutral score (never blocks the benchmark).
        Err(_) => 5.0,
    }
}

/// Call a model by alias through a **non-OpenRouter** provider so judging and
/// calibration never consume the 300/day OpenRouter budget. Default judge:
/// `gpt-oss-120b` (Groq/Cerebras).
async fn call_non_openrouter(
    client: &reqwest::Client,
    alias: &str,
    prompt: &str,
    db: Option<Arc<SqliteStore>>,
) -> anyhow::Result<String> {
    let gemini_key = std::env::var("GEMINI_API_KEY").unwrap_or_default();
    let models = crate::crypto::ai::providers::available_models();
    let mut last_err = anyhow::anyhow!("No non-OpenRouter provider serves alias {alias}");
    for info in models
        .iter()
        .filter(|m| m.alias == alias && m.provider != Provider::OpenRouter)
    {
        match call_single_model(client, &gemini_key, info, prompt, db.clone()).await {
            Ok(res) => return Ok(res),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// Call the judge model (non-OpenRouter by construction).
async fn call_judge(
    client: &reqwest::Client,
    prompt: &str,
    db: Option<Arc<SqliteStore>>,
) -> anyhow::Result<String> {
    let alias =
        std::env::var("OPENROUTER_JUDGE_MODEL").unwrap_or_else(|_| "gpt-oss-120b".to_string());
    call_non_openrouter(client, &alias, prompt, db).await
}

/// Run the reference models through the judge to verify it is not biased.
/// Returns the average reference score; a value below [`JUDGE_MIN_BASELINE`]
/// means the judge is unreliable and role assignment should be skipped.
pub async fn calibrate(client: &reqwest::Client, db: &Arc<SqliteStore>) -> f64 {
    let refs = ["gpt-oss-120b", "command-a-reasoning"];
    let mut scores = Vec::new();
    for alias in refs {
        let prompt = sample_prompt("main").to_string();
        // Call the reference model itself, then score its answer with the judge.
        if let Ok(resp) = call_non_openrouter(client, alias, &prompt, Some(db.clone())).await {
            let jp = judge_prompt("main", &prompt, &resp);
            if let Ok(score_text) = call_judge(client, &jp, Some(db.clone())).await {
                if let Some(s) = parse_judge_score(&score_text) {
                    scores.push(s);
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let baseline = if scores.is_empty() {
        0.0
    } else {
        scores.iter().sum::<f64>() / scores.len() as f64
    };
    // Persist best-effort for drift tracking.
    let _ = std::fs::write(
        "judge_baseline.json",
        serde_json::json!({
            "baseline": baseline,
            "scores": scores,
            "at": chrono::Utc::now().to_rfc3339(),
        })
        .to_string(),
    );
    baseline
}

// ── Role assignment ──────────────────────────────────────────────────────────

/// Assign roles/priorities to OpenRouter models from accumulated stats.
/// Only models with real data (bench or production) are considered; flaky
/// models (error rate ≥ 50%) are excluded.
fn assign_roles(
    registry: &[(
        String,
        String,
        String,
        String,
        i64,
        String,
        Option<String>,
        String,
        i64,
    )],
    stats: &[(String, String, i64, i64, i64, f64)],
) -> Vec<ModelConfig> {
    let mut scored: Vec<(f64, i64, ModelConfig)> = Vec::new();
    for (provider, model_id, alias, display, _ctx, _fp, _tested, _tfp, _bc) in registry {
        if provider != "OpenRouter" {
            continue;
        }
        let Some((calls, errors, latency, quality)) = stats_for(provider, model_id, stats) else {
            continue;
        };
        if calls == 0 {
            continue;
        }
        let err_rate = errors as f64 / calls as f64;
        if err_rate >= 0.5 {
            continue; // too flaky to trust
        }
        // Bench quality is authoritative; production-only models get neutral 5.0.
        let base = if quality > 0.0 { quality } else { 5.0 };
        let effective = (base * (1.0 - 0.5 * err_rate)).clamp(0.0, 10.0);
        let roles: Vec<String> = ROLES
            .iter()
            .filter(|r| effective >= role_threshold(r))
            .map(|r| r.to_string())
            .collect();
        if roles.is_empty() {
            continue;
        }
        scored.push((
            effective,
            latency,
            ModelConfig {
                id: model_id.clone(),
                display: display.clone(),
                alias: alias.clone(),
                provider: provider.clone(),
                roles,
                priority: 0, // filled below
            },
        ));
    }

    // Global ranking: quality desc, then latency asc.
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    let assigned: Vec<ModelConfig> = scored
        .into_iter()
        .enumerate()
        .map(|(i, (_, _, mut cfg))| {
            cfg.priority = i as i32 + 1;
            cfg
        })
        .collect();
    assigned
}

fn write_rank_file(assigned: &[ModelConfig]) -> anyhow::Result<()> {
    let file = ModelsFile {
        models: assigned.to_vec(),
    };
    let js = serde_json::to_string_pretty(&file)?;
    let tmp = format!("{RANK_FILE}.tmp");
    std::fs::write(&tmp, js)?;
    std::fs::rename(&tmp, RANK_FILE)?;
    Ok(())
}

// ── Public entry points ──────────────────────────────────────────────────────

/// Run one lazy benchmark batch: select ≤ [`MAX_BENCH_PER_RUN`] candidates,
/// calibrate the judge, benchmark each (sleeping between calls), then rewrite
/// the OpenRouter rank file.
pub async fn rank(client: &reqwest::Client, db: &Arc<SqliteStore>) -> anyhow::Result<RankReport> {
    let (used, budget) = crate::crypto::ai::openrouter_budget::OpenRouterBudget::instance().usage();
    let remaining = budget - used;

    let registry = db.get_model_registry()?;
    let stats = db.get_model_stats(STATS_WINDOW_DAYS)?;
    let candidates = select_candidates(&registry, &stats, remaining);

    let judge_baseline = calibrate(client, db).await;
    let judge_reliable = judge_baseline >= JUDGE_MIN_BASELINE;

    let mut tested = Vec::new();
    let mut skipped_tested = 0;
    for cand in candidates {
        // Re-check the budget before every OpenRouter call — production always
        // wins over benchmarking.
        if cand.provider == "OpenRouter" {
            let (u, b) = crate::crypto::ai::openrouter_budget::OpenRouterBudget::instance().usage();
            if b - u < MIN_OPENROUTER_RESERVE {
                skipped_tested += 1;
                continue;
            }
        }
        let result = bench_one(client, db, &cand).await;
        if result.skipped_rate_limit {
            skipped_tested += 1;
        }
        tested.push(result);
        // Gentle pacing: 2-5s between calls, never a burst.
        tokio::time::sleep(Duration::from_secs(2 + (tested.len() as u64 % 4))).await;
    }

    let mut assigned = Vec::new();
    if judge_reliable {
        // Re-read stats so this run's bench results feed the assignment.
        let stats = db.get_model_stats(STATS_WINDOW_DAYS)?;
        assigned = assign_roles(&registry, &stats);
        if let Err(e) = write_rank_file(&assigned) {
            tracing::warn!("model ranker: failed to write {RANK_FILE}: {e}");
        }
    } else {
        tracing::warn!(
            "model ranker: judge baseline {judge_baseline:.1} < {JUDGE_MIN_BASELINE}, skipping role assignment"
        );
    }

    Ok(RankReport {
        tested,
        skipped_tested,
        assigned,
        judge_baseline,
        judge_reliable,
        budget_remaining: remaining,
    })
}

/// Aggregate stats over the last N days: (provider, model_id, calls, errors,
/// avg_latency_ms, avg_quality).
pub fn stats(
    db: &SqliteStore,
    days: u32,
) -> anyhow::Result<Vec<(String, String, i64, i64, i64, f64)>> {
    db.get_model_stats(days)
}

// ── Prompts ──────────────────────────────────────────────────────────────────

fn sample_prompt(role: &str) -> &'static str {
    match role {
        "critic" => "Critique this investment thesis in 3 sentences: \
            \"Buy LINK now because it rose 30% in 7 days.\" Point out the flaws.",
        "news" => "Summarize this headline in one concise sentence: \
            \"SEC proposes safe-harbor rules for token issuers.\"",
        "watcher" => "Classify this market state in one sentence: \
            BTC +2% in 24h, ETH +5%, Fear & Greed 55. Is it a dip-buy moment? Answer yes or no with one reason.",
        _ => "Explain in 3 sentences what drives the crypto market this week \
            and what a long-term spot investor should watch.",
    }
}

fn judge_prompt(role: &str, sample: &str, response: &str) -> String {
    format!(
        "You are a strict model-quality judge. A model was asked to perform the \
         role '{role}' with this task:\n\n{sample}\n\nIts response:\n\n{response}\n\n\
         Score the response 0-10 on: correctness, relevance to the role, reasoning \
         depth, and absence of hallucination. Reply with ONLY a JSON object: \
         {{\"score\": <number>, \"notes\": \"<one short line>\"}}"
    )
}

fn parse_judge_score(text: &str) -> Option<f64> {
    let cleaned = crate::crypto::ai::providers::clean_model_output(text);
    // Try JSON first.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&cleaned) {
        if let Some(s) = v["score"].as_f64() {
            return Some(s.clamp(0.0, 10.0));
        }
    }
    // Fallback: the first number in the text.
    let mut num = String::new();
    let mut found = false;
    for c in cleaned.trim().chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            found = true;
        } else if found {
            break;
        }
    }
    if found {
        num.parse::<f64>().ok().map(|s: f64| s.clamp(0.0, 10.0))
    } else {
        None
    }
}

fn provider_name(raw: &str) -> String {
    match Provider::from_str(raw) {
        Ok(p) => format!("{:?}", p),
        Err(_) => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_thresholds_are_ordered() {
        assert!(role_threshold("main") > role_threshold("news"));
        assert!(role_threshold("news") > role_threshold("watcher"));
        assert_eq!(role_threshold("critic"), role_threshold("main"));
    }

    #[test]
    fn sample_prompts_exist_for_all_roles() {
        for r in ROLES {
            assert!(!sample_prompt(r).is_empty(), "missing prompt for {r}");
        }
    }

    #[test]
    fn judge_prompt_contains_role_and_response() {
        let p = judge_prompt("main", "task", "answer");
        assert!(p.contains("main"));
        assert!(p.contains("task"));
        assert!(p.contains("answer"));
    }

    #[test]
    fn parse_judge_score_json_and_bare() {
        assert_eq!(
            parse_judge_score(r#"{"score": 7.5, "notes": "ok"}"#),
            Some(7.5)
        );
        assert_eq!(parse_judge_score("8"), Some(8.0));
        assert_eq!(parse_judge_score("score: 9.2 out of 10"), Some(9.2));
        assert_eq!(parse_judge_score("nonsense"), None);
        // Out-of-range clamps.
        assert_eq!(parse_judge_score(r#"{"score": 42}"#), Some(10.0));
    }

    #[test]
    fn rate_limit_detection() {
        for msg in [
            "HTTP 429: Too Many Requests",
            "rate limit exceeded for model",
            "OpenRouter: quota exceeded",
            "provider overloaded, try again later",
        ] {
            assert!(is_rate_limit(&anyhow::anyhow!("{msg}")), "missed: {msg}");
        }
        assert!(!is_rate_limit(&anyhow::anyhow!("HTTP 500: internal error")));
        assert!(!is_rate_limit(&anyhow::anyhow!("invalid api key")));
    }

    #[test]
    fn select_candidates_respects_cooldown_and_budget() {
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let old = chrono::Utc::now()
            .checked_sub_signed(chrono::Duration::days(40))
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();

        // (provider, model_id, alias, display, ctx, fingerprint, last_tested, tested_fp, bench_count)
        let registry = vec![
            (
                "OpenRouter".into(),
                "new-model".into(),
                "new-model".into(),
                "New".into(),
                0,
                "100".into(),
                None,
                "".into(),
                0,
            ),
            (
                "OpenRouter".into(),
                "fresh-model".into(),
                "fresh-model".into(),
                "Fresh".into(),
                0,
                "100".into(),
                Some(now.clone()),
                "100".into(),
                3,
            ),
            (
                "OpenRouter".into(),
                "old-model".into(),
                "old-model".into(),
                "Old".into(),
                0,
                "100".into(),
                Some(old.clone()),
                "100".into(),
                2,
            ),
            (
                "OpenRouter".into(),
                "changed-model".into(),
                "changed-model".into(),
                "Changed".into(),
                0,
                "200".into(),
                Some(now.clone()),
                "100".into(),
                5,
            ),
        ];

        // No budget → no OpenRouter candidates at all.
        let c = select_candidates(&registry, &[], 0);
        assert!(c.is_empty(), "must not bench with zero budget headroom");

        // Plenty of budget → new + changed + cooldown-expired, capped at 3.
        let c = select_candidates(&registry, &[], 100);
        assert_eq!(c.len(), 3, "batch must be capped at MAX_BENCH_PER_RUN");
        let reasons: Vec<&str> = c.iter().map(|x| x.reason).collect();
        assert!(reasons.contains(&"new"));
        assert!(reasons.contains(&"changed"));
        assert!(reasons.contains(&"cooldown-expired"));
        // The freshly-tested unchanged model must NOT appear.
        assert!(!c.iter().any(|x| x.model_id == "fresh-model"));
    }

    #[test]
    fn select_candidates_demotes_underperformers() {
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let registry = vec![(
            "Groq".into(),
            "flaky".into(),
            "flaky".into(),
            "Flaky".into(),
            0,
            "".into(),
            Some(now.clone()),
            "".into(),
            4,
        )];
        // 20 calls, 8 errors → 40% error rate > 30% threshold.
        let stats = vec![("Groq".into(), "flaky".into(), 20, 8, 5000, 0.0)];
        let c = select_candidates(&registry, &stats, 100);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].reason, "underperforming");
        // Healthy model → no re-test.
        let stats_ok = vec![("Groq".into(), "flaky".into(), 20, 2, 5000, 0.0)];
        assert!(select_candidates(&registry, &stats_ok, 100).is_empty());
    }

    #[test]
    fn assign_roles_ranks_and_filters() {
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let registry = vec![
            (
                "OpenRouter".into(),
                "good".into(),
                "good".into(),
                "Good".into(),
                0,
                "".into(),
                Some(now.clone()),
                "".into(),
                1,
            ),
            (
                "OpenRouter".into(),
                "flaky".into(),
                "flaky".into(),
                "Flaky".into(),
                0,
                "".into(),
                Some(now.clone()),
                "".into(),
                1,
            ),
            (
                "Groq".into(),
                "hand-configured".into(),
                "hand-configured".into(),
                "Hand".into(),
                0,
                "".into(),
                Some(now.clone()),
                "".into(),
                1,
            ),
        ];
        // good: quality 8, 0 errors → main+critic+news+watcher.
        // flaky: 60% errors → excluded entirely.
        let stats = vec![
            ("OpenRouter".into(), "good".into(), 10, 0, 1000, 8.0),
            ("OpenRouter".into(), "flaky".into(), 10, 6, 2000, 7.0),
        ];
        let assigned = assign_roles(&registry, &stats);
        assert_eq!(
            assigned.len(),
            1,
            "flaky and non-OpenRouter must be excluded"
        );
        assert_eq!(assigned[0].id, "good");
        assert_eq!(assigned[0].priority, 1);
        assert!(assigned[0].roles.contains(&"main".to_string()));
        assert!(assigned[0].roles.contains(&"watcher".to_string()));
    }

    #[test]
    fn provider_name_maps_enum() {
        assert_eq!(provider_name("openrouter"), "OpenRouter");
        assert_eq!(provider_name("groq"), "Groq");
        assert_eq!(provider_name("weird"), "weird");
    }
}
