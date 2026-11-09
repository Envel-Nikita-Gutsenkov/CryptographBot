/// Per-critic wall-clock slice. Reasoning-capable critics need more than the
/// old 25s (deepseek-v4-flash timed out on every /ask, so the pipeline always
/// fell back to the raw generator draft). Kept in one place so the pipeline
/// budget and the log lines cannot drift apart.
pub const CRITIC_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(40);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Provider {
    Gemini,
    Mistral,
    Groq,
    Nvidia,
    OpenRouter,
    Cohere,
    Cerebras,
    HuggingFace,
    Cloudflare,
    DeepSeek,
}

impl std::str::FromStr for Provider {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "gemini" => Ok(Provider::Gemini),
            "mistral" => Ok(Provider::Mistral),
            "groq" => Ok(Provider::Groq),
            "nvidia" => Ok(Provider::Nvidia),
            "openrouter" => Ok(Provider::OpenRouter),
            "cohere" => Ok(Provider::Cohere),
            "cerebras" => Ok(Provider::Cerebras),
            "huggingface" | "hf" => Ok(Provider::HuggingFace),
            "cloudflare" | "cf" => Ok(Provider::Cloudflare),
            "deepseek" | "ds" => Ok(Provider::DeepSeek),
            _ => Err(()),
        }
    }
}

pub struct ModelInfo {
    pub id: String,
    pub display: String,
    pub provider: Provider,
    pub alias: String,
}

pub fn available_models() -> Vec<ModelInfo> {
    let models = crate::crypto::ai::pools::load_all_models();
    models
        .into_iter()
        .map(|m| {
            let provider = std::str::FromStr::from_str(&m.provider).unwrap_or(Provider::Gemini);
            ModelInfo {
                id: m.id,
                display: m.display,
                provider,
                alias: m.alias,
            }
        })
        .collect()
}

pub async fn call_any_model(
    client: &reqwest::Client,
    gemini_key: &str,
    alias: &str,
    prompt: &str,
    db: Option<std::sync::Arc<crate::db::sqlite::SqliteStore>>,
) -> anyhow::Result<String> {
    if std::env::var("CRYPTOGRAPH_DEBUG_PROMPTS").unwrap_or_default() == "1" {
        println!(">> [AI Model Call] {}", alias);
    }

    let models = available_models();
    let infos: Vec<_> = models.iter().filter(|m| m.alias == alias).collect();
    if infos.is_empty() {
        return Err(anyhow::anyhow!("Unknown model alias {alias}"));
    }

    let mut last_err = anyhow::anyhow!("No providers worked for {alias}");
    for info in infos {
        match call_single_model(client, gemini_key, info, prompt, db.clone()).await {
            Ok(res) => return Ok(res),
            Err(e) => last_err = e,
        }
    }

    Err(last_err)
}

/// Provider name used by the health table and the in-memory circuit breaker,
/// e.g. `"Cloudflare"`, `"Groq"` (matches `format!("{:?}", provider)`).
fn provider_id_provider(p: &Provider) -> String {
    format!("{p:?}")
}

/// Call exactly ONE provider/model entry (no alias expansion).
/// Used by the health worker to attribute errors to the correct provider
/// and as the internal dispatch for `call_any_model`.
pub async fn call_single_model(
    client: &reqwest::Client,
    gemini_key: &str,
    info: &ModelInfo,
    prompt: &str,
    db: Option<std::sync::Arc<crate::db::sqlite::SqliteStore>>,
) -> anyhow::Result<String> {
    // Check dynamic health from DB
    let provider_id = format!("{:?}/{}", info.provider, info.id);
    if let Some(store) = &db {
        if let Ok(status) = store.get_model_health(&provider_id) {
            if status == "DEAD" || status == "COOLDOWN" {
                tracing::warn!(
                    "Skipping model {} because health status is {}",
                    provider_id,
                    status
                );
                return Err(anyhow::anyhow!(
                    "Circuit breaker active (DB status: {})",
                    status
                ));
            }
        }
    }

    if crate::crypto::ai::pools::is_banned(&info.provider, info.id.as_str()) {
        tracing::warn!(
            "Skipping provider {:?} for model {} (in-memory circuit breaker active)",
            info.provider,
            info.id
        );
        return Err(anyhow::anyhow!(
            "In-memory circuit breaker active for {:?}",
            info.provider
        ));
    }

    // Key-wide free quota already gone (Cloudflare neurons, Groq TPD, …)?
    // Every model on that provider is dead until the quota resets, so fail fast
    // instead of paying a network round-trip (and a 10 s timeout) for nothing.
    if crate::crypto::ai::pools::is_provider_exhausted(&provider_id_provider(&info.provider)) {
        return Err(anyhow::anyhow!(
            "Provider {:?} quota exhausted until reset",
            info.provider
        ));
    }

    let started = std::time::Instant::now();
    let mut attempts = 0;
    let result = loop {
        attempts += 1;
        let res = dispatch_provider_call(client, gemini_key, info, prompt).await;
        match &res {
            Err(e) if attempts < 3 => {
                let err_s = e.to_string();
                // Quota exhaustion (daily neurons/TPD, provider-wide) does not
                // recover in 500 ms — retrying it wastes the latency budget.
                if crate::crypto::ai::pools::quota_cooldown_secs(&err_s).is_some() {
                    break res;
                }
                // Transient failures only: timeouts, 5xx, and 429 rate limits.
                // Anything else (auth, 4xx, bad payload) fails immediately.
                let transient = err_s.contains("timed out")
                    || err_s.contains("502")
                    || err_s.contains("503")
                    || err_s.contains("504")
                    || err_s.contains("429")
                    || err_s.contains("rate limit")
                    || err_s.contains("too many requests");
                if transient {
                    // Exponential backoff: 500ms, 1s, 2s — bounded so a spike
                    // costs at most ~3.5s of latency instead of panicking.
                    let backoff_ms = 500u64 * (1u64 << (attempts - 1));
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    continue;
                }
            }
            _ => {}
        }
        break res;
    };
    let latency_ms = started.elapsed().as_millis() as i64;

    // Production ok/err/latency feed model_stats (quality=0 — benchmarks are the
    // only quality source). Best-effort: a stats write must never break a call.
    let provider_str = format!("{:?}", info.provider);
    if let Some(store) = &db {
        let ok = result.is_ok();
        let _ = store.record_model_stats(&provider_str, &info.id, latency_ms, ok, 0.0);
    }
    // Feed the per-day dead-model detector regardless of DB availability.
    crate::crypto::ai::pools::record_model_outcome(&info.alias, result.is_ok());

    match result {
        Ok(res) => {
            let cleaned = clean_model_output(&res);
            crate::crypto::ai::metrics::record_success(&provider_id);
            // Real success → mark healthy (clears any stale ban).
            if let Some(store) = &db {
                store.update_model_health(&provider_id, "OK", None);
            }
            Ok(cleaned)
        }
        Err(e) => {
            let err_s = e.to_string();
            // A prompt that does not fit a free tier's TPM gate is not a provider
            // fault: smaller interactive prompts still work, so neither the model
            // nor the provider should be benched for it. Debug level because this
            // is expected for the big news prompts (Groq allows 8k TPM).
            if err_s.contains("prompt too large") {
                crate::crypto::ai::metrics::record_error(&provider_id);
                tracing::debug!(
                    "Provider {:?} skipped for model {}: {}",
                    info.provider,
                    info.id,
                    err_s
                );
                return Err(e);
            }
            tracing::warn!(
                "Provider {:?} failed for model {}: {}",
                info.provider,
                info.id,
                e
            );
            crate::crypto::ai::metrics::record_error(&provider_id);
            // Permanent failures (auth/4xx) bench the model for 1h in-memory so
            // dead models stop being retried every 60s. Transient ones (timeout,
            // 5xx, 429, Cohere 422) get NO ban: the model is fine, just busy —
            // banning it would let background workers knock working models out
            // of the pool for interactive /ask calls.
            // Quota exhaustion → bench the whole provider until its reset time,
            // so background workers stop hammering it every minute. Burst 429s
            // (no quota marker) keep the existing short/1h behaviour.
            if let Some(secs) = crate::crypto::ai::pools::quota_cooldown_secs(&err_s) {
                // Gemini free quota is per model *and* per project: a 429 on
                // gemini-3.5-flash says nothing about gemini-2.5-flash, which
                // uses a separate bucket. Benching the whole provider there used
                // to knock healthy Gemini models out of the pool for the day.
                // Key-wide providers (Cloudflare neurons, Groq TPD) keep the
                // provider-wide bench.
                if info.provider != Provider::Gemini {
                    crate::crypto::ai::pools::mark_provider_exhausted(
                        &provider_str,
                        std::time::Instant::now() + std::time::Duration::from_secs(secs),
                    );
                }
                crate::crypto::ai::pools::ban_provider_model(
                    &info.provider,
                    info.id.as_str(),
                    secs,
                );
                if let Some(store) = &db {
                    store.update_model_health(&provider_id, "COOLDOWN", Some(err_s.clone()));
                }
                return Err(e);
            }
            let transient = err_s.contains("timed out")
                || err_s.contains("502")
                || err_s.contains("503")
                || err_s.contains("504")
                || err_s.contains("429")
                || err_s.contains("422")
                || err_s.contains("rate limit")
                || err_s.contains("too many requests");
            if !transient {
                crate::crypto::ai::pools::ban_provider_model(
                    &info.provider,
                    info.id.as_str(),
                    3600,
                );
            }
            // Real failure → temporarily bench the model (1h COOLDOWN in DB,
            // 1h in-memory circuit breaker for permanent errors only). Health
            // comes from real traffic only.
            if let Some(store) = &db {
                store.update_model_health(&provider_id, "COOLDOWN", Some(e.to_string()));
            }
            Err(e)
        }
    }
}

/// One raw provider/model call with no circuit-breaker, ban, quota or stats
/// side effects. `call_single_model` wraps this with all the pool bookkeeping;
/// the live probe calls it directly so it always sees the real provider answer.
async fn dispatch_provider_call(
    client: &reqwest::Client,
    gemini_key: &str,
    info: &ModelInfo,
    prompt: &str,
) -> anyhow::Result<String> {
    match info.provider {
        Provider::Gemini => {
            let body = serde_json::json!({
                "contents": [{"parts": [{"text": prompt}]}]
            });
            call_gemini_model(client, gemini_key, info.id.as_str(), &body).await
        }
        Provider::Groq => call_groq_model(client, info.id.as_str(), prompt).await,
        Provider::Mistral => call_mistral_model(client, info.id.as_str(), prompt).await,
        Provider::Nvidia => call_nvidia_model(client, info.id.as_str(), prompt).await,
        Provider::OpenRouter => call_openrouter_raw(client, info.id.as_str(), prompt).await,
        Provider::Cohere => call_cohere_model(client, info.id.as_str(), prompt).await,
        Provider::Cerebras => call_cerebras_model(client, info.id.as_str(), prompt).await,
        Provider::HuggingFace => call_huggingface_model(client, info.id.as_str(), prompt).await,
        Provider::Cloudflare => call_cloudflare_model(client, info.id.as_str(), prompt).await,
        Provider::DeepSeek => call_deepseek_model(info.id.as_str(), prompt).await,
    }
}

/// Last-resort fallback used when the whole model pool is dead/skipped (rate
/// limits, quota exhaustion, circuit breakers).
///
/// Runs independent providers **in parallel waves** instead of one after
/// another: the old sequential chain spent up to 20s per dead provider, so a
/// single 429 early in the list (Cohere's monthly trial cap) could burn the
/// whole /ask budget before DeepSeek was even tried — which is exactly how
/// "лимиты исчерпаны" appeared while a healthy provider was still available.
/// Returns (provider_label, text) on first success.
pub async fn call_last_resort(
    client: &reqwest::Client,
    gemini_key: &str,
    prompt: &str,
) -> anyhow::Result<(String, String)> {
    use futures::future::BoxFuture;
    use futures::stream::{FuturesUnordered, StreamExt};

    type Job = BoxFuture<'static, Option<(String, String)>>;

    // Each job is bounded and self-contained: it skips providers whose key-wide
    // quota is already gone and records quota/auth failures for the pool.
    fn job<F>(label: &'static str, provider: &'static str, fut: F) -> Job
    where
        F: std::future::Future<Output = anyhow::Result<String>> + Send + 'static,
    {
        Box::pin(async move {
            if crate::crypto::ai::pools::is_provider_exhausted(provider) {
                return None;
            }
            match tokio::time::timeout(std::time::Duration::from_secs(45), fut).await {
                Ok(Ok(res)) => {
                    crate::crypto::ai::pools::record_model_outcome(label, true);
                    Some((label.to_string(), res))
                }
                Ok(Err(e)) => {
                    let err_s = e.to_string();
                    crate::crypto::ai::pools::record_model_outcome(label, false);
                    if let Some(secs) = crate::crypto::ai::pools::quota_cooldown_secs(&err_s) {
                        crate::crypto::ai::pools::mark_provider_exhausted(
                            provider,
                            std::time::Instant::now() + std::time::Duration::from_secs(secs),
                        );
                    } else if err_s.contains("401") || err_s.contains("User not found") {
                        // A revoked/invalid key never recovers on its own. Park it
                        // for a day instead of probing it on every fallback.
                        crate::crypto::ai::pools::mark_provider_exhausted(
                            provider,
                            std::time::Instant::now() + std::time::Duration::from_secs(86_400),
                        );
                    }
                    tracing::warn!("Last-resort {} failed: {}", label, e);
                    None
                }
                Err(_) => {
                    tracing::warn!("Last-resort {} timed out after 45s", label);
                    None
                }
            }
        })
    }

    /// Run one parallel wave; return the first success or None if all failed.
    async fn wave(jobs: Vec<Job>) -> Option<(String, String)> {
        let mut set: FuturesUnordered<Job> = jobs.into_iter().collect();
        while let Some(res) = set.next().await {
            if let Some(ok) = res {
                return Some(ok);
            }
        }
        None
    }

    // Wave 1: independent vendors with their own quota buckets. DeepSeek is the
    // most reliable paid key; Mistral/Groq/Cloudflare cover the free tiers.
    let (c, gp) = (client.clone(), prompt.to_string());
    let gk = gemini_key.to_string();
    let gemini_body = serde_json::json!({
        "contents": [{"parts": [{"text": prompt}]}]
    });

    let mut w1: Vec<Job> = Vec::new();
    w1.push(job("deepseek-v4-flash", "DeepSeek", {
        let p = gp.clone();
        async move { call_deepseek_model("deepseek-v4-flash", &p).await }
    }));
    w1.push(job("ministral-14b", "Mistral", {
        let (c, p) = (c.clone(), gp.clone());
        async move { call_mistral_model(&c, "ministral-14b-latest", &p).await }
    }));
    w1.push(job("gpt-oss-120b", "Groq", {
        let (c, p) = (c.clone(), gp.clone());
        async move { call_groq_model(&c, "openai/gpt-oss-120b", &p).await }
    }));
    w1.push(job("llama-4-scout-cf", "Cloudflare", {
        let (c, p) = (c.clone(), gp.clone());
        async move {
            call_cloudflare_model(&c, "@cf/meta/llama-4-scout-17b-16e-instruct", &p).await
        }
    }));
    if let Some(r) = wave(w1).await {
        return Ok(r);
    }

    // Wave 2: second models plus the easily-exhausted free tiers. Reaching this
    // means wave 1 failed or was fully benched, so the extra calls are justified.
    let mut w2: Vec<Job> = Vec::new();
    w2.push(job("mistral-large-4", "Mistral", {
        let (c, p) = (c.clone(), gp.clone());
        async move { call_mistral_model(&c, "mistral-large-4", &p).await }
    }));
    w2.push(job("gpt-oss-20b", "Groq", {
        let (c, p) = (c.clone(), gp.clone());
        async move { call_groq_model(&c, "openai/gpt-oss-20b", &p).await }
    }));
    w2.push(job("qwen3-30b-cf", "Cloudflare", {
        let (c, p) = (c.clone(), gp.clone());
        async move { call_cloudflare_model(&c, "@cf/qwen/qwen3-30b-a3b-fp8", &p).await }
    }));
    w2.push(job("command-a-reasoning", "Cohere", {
        let (c, p) = (c.clone(), gp.clone());
        async move { call_cohere_model(&c, "command-a-reasoning-08-2025", &p).await }
    }));
    for gemini_model in ["gemini-2.5-flash-lite", "gemini-2.5-flash"] {
        let body = gemini_body.clone();
        let (c, k) = (c.clone(), gk.clone());
        let label: &'static str = if gemini_model == "gemini-2.5-flash-lite" {
            "gemini-2.5-flash-lite"
        } else {
            "gemini-2.5-flash"
        };
        w2.push(job(label, "Gemini", async move {
            call_gemini_model(&c, &k, gemini_model, &body).await
        }));
    }
    if let Some(r) = wave(w2).await {
        return Ok(r);
    }

    Err(anyhow::anyhow!("All last-resort providers failed"))
}

pub async fn call_gemini_model(
    client: &reqwest::Client,
    api_key: &str,
    model_id: &str,
    body: &serde_json::Value,
) -> anyhow::Result<String> {
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        model_id, api_key
    );
    let resp = client.post(&url).json(body).send().await?;

    if resp.status().is_success() {
        let json = resp.json::<serde_json::Value>().await?;
        return json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("Failed to parse Gemini response for {model_id}"));
    } else {
        let status = resp.status().as_u16();
        let err_body = resp.text().await.unwrap_or_default();
        let truncated = truncate_error_body(&err_body);
        return Err(anyhow::anyhow!(
            "Gemini {model_id} HTTP {status}: {truncated}"
        ));
    }
}

/// Rough token estimate for the free tiers' TPM gates. English+JSON prompts
/// average ~3 chars per token for BPE tokenizers, so dividing by 3 never
/// under-counts the risky direction (over-counting only skips a call early).
pub(crate) fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 3 + 1
}

async fn call_groq_model(
    client: &reqwest::Client,
    model: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let key = std::env::var("GROQ_API_KEY").unwrap_or_default();
    // The on-demand tier is capped at 8k TPM *including* the completion. A
    // larger prompt can only ever answer 413, so skip the HTTP call instead of
    // burning quota round-trips on requests that are refused by construction.
    let tokens = estimate_tokens(prompt);
    if tokens > 6000 {
        return Err(anyhow::anyhow!(
            "prompt too large for Groq TPM limit (~{tokens} tokens, cap 8000/min)"
        ));
    }
    if key.is_empty() {
        return Err(anyhow::anyhow!("GROQ_API_KEY not configured"));
    }
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 4096,
        "temperature": 0.3
    });
    let resp = client
        .post("https://api.groq.com/openai/v1/chat/completions")
        .header("Authorization", format!("Bearer {key}"))
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let err = resp.text().await.unwrap_or_default();
        let truncated = truncate_error_body(&err);
        return Err(anyhow::anyhow!("Groq {model} HTTP {status}: {truncated}"));
    }
    let json = resp.json::<serde_json::Value>().await?;
    extract_message_content(&json).ok_or_else(|| {
        anyhow::anyhow!(
            "Failed to parse Groq response for {model}: {}",
            truncate_error_body(&json.to_string())
        )
    })
}

async fn call_mistral_model(
    client: &reqwest::Client,
    model: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let key = std::env::var("MISTRAL_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Err(anyhow::anyhow!("MISTRAL_API_KEY not configured"));
    }
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}]
    });
    let resp = client
        .post("https://api.mistral.ai/v1/chat/completions")
        .header("Authorization", format!("Bearer {key}"))
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        let truncated = truncate_error_body(&body);
        return Err(anyhow::anyhow!(
            "Mistral {model} HTTP {status}: {truncated}"
        ));
    }
    let json = resp.json::<serde_json::Value>().await?;
    extract_message_content(&json).ok_or_else(|| {
        anyhow::anyhow!(
            "Failed to parse Mistral response for {model}: {}",
            truncate_error_body(&json.to_string())
        )
    })
}

async fn call_nvidia_model(
    client: &reqwest::Client,
    model: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let key = std::env::var("NVIDIA_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Err(anyhow::anyhow!("NVIDIA_API_KEY not configured"));
    }
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 1.0,
        "top_p": 0.95,
        "max_tokens": 8192,
        "stream": false,
    });
    let resp = client
        .post("https://integrate.api.nvidia.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {key}"))
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        let truncated = truncate_error_body(&body);
        return Err(anyhow::anyhow!("Nvidia {model} HTTP {status}: {truncated}"));
    }
    let json = resp.json::<serde_json::Value>().await?;
    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Failed to parse Nvidia response for {model}"))
}

/// Budgeted OpenRouter call (daily 300-request cap). Public so the model
/// ranker can benchmark free models through the exact same path as production.
pub(crate) async fn call_openrouter_raw(
    client: &reqwest::Client,
    model: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Err(anyhow::anyhow!("OPENROUTER_API_KEY not configured"));
    }

    // Serialize OpenRouter calls so budget check + settle are race-free (hard
    // daily cap — the free tier allows 300 requests/day on the shared key).
    static CALL_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = CALL_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;

    let budget = super::openrouter_budget::OpenRouterBudget::instance();
    budget
        .reserve()
        .map_err(|e| anyhow::anyhow!("OpenRouter budget: {}", e))?;

    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0.7,
        "max_tokens": 8192,
    });
    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .header("Authorization", format!("Bearer {key}"))
        .header("HTTP-Referer", "https://github.com/Cryptograph")
        .header("X-Title", "Cryptograph Bot")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        let truncated = truncate_error_body(&body);
        return Err(anyhow::anyhow!(
            "OpenRouter {model} HTTP {status}: {truncated}"
        ));
    }
    let json = resp.json::<serde_json::Value>().await?;

    // Commit actual usage (prefer API-reported totals).
    let used = json["usage"]["total_tokens"]
        .as_i64()
        .unwrap_or_else(|| (prompt.len() as i64 / 4).max(1));
    budget.settle(used.max(1));
    budget.record_daily(model, used.max(1));

    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Failed to parse OpenRouter response for {model}"))
}

async fn call_cohere_model(
    client: &reqwest::Client,
    model: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let key = std::env::var("COHERE_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Err(anyhow::anyhow!("COHERE_API_KEY not configured"));
    }
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}]
    });
    let resp = client
        .post("https://api.cohere.com/v2/chat")
        .header("Authorization", format!("Bearer {key}"))
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "Cohere {} HTTP {}: {}",
            model,
            status,
            truncate_error_body(&body)
        ));
    }
    let json = resp.json::<serde_json::Value>().await?;

    // Cohere v2 models might return a "thinking" block before the "text" block.
    if let Some(content_array) = json["message"]["content"].as_array() {
        for item in content_array {
            if item["type"].as_str() == Some("text") {
                if let Some(text) = item["text"].as_str() {
                    return Ok(text.to_string());
                }
            }
        }
    }
    Err(anyhow::anyhow!("Failed to parse Cohere v2 response"))
}

async fn call_cerebras_model(
    client: &reqwest::Client,
    model: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let key = std::env::var("CEREBRAS_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Err(anyhow::anyhow!("CEREBRAS_API_KEY not configured"));
    }
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0.2,
        "max_completion_tokens": 8192,
        "top_p": 1.0,
        "reasoning_effort": "medium",
        "stream": false
    });
    let resp = client
        .post("https://api.cerebras.ai/v1/chat/completions")
        .header("Authorization", format!("Bearer {key}"))
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "Cerebras {} HTTP {}: {}",
            model,
            status,
            truncate_error_body(&body)
        ));
    }
    let json = resp.json::<serde_json::Value>().await?;
    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Failed to parse Cerebras response"))
}

async fn call_huggingface_model(
    client: &reqwest::Client,
    model: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let key = std::env::var("HF_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Err(anyhow::anyhow!("HF_API_KEY not configured"));
    }
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0.2,
        "max_tokens": 8192,
        "top_p": 0.9,
    });
    let url = format!(
        "https://api-inference.huggingface.co/models/{}/v1/chat/completions",
        model
    );
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {key}"))
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "HuggingFace {} HTTP {}: {}",
            model,
            status,
            truncate_error_body(&body)
        ));
    }
    let json = resp.json::<serde_json::Value>().await?;
    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Failed to parse HuggingFace response"))
}

/// Extract the assistant text from an OpenAI-compatible chat response.
///
/// Some providers (Mistral among them) return `content` as an array of
/// `{"type":"text","text":...}` chunks instead of a plain string, and
/// reasoning models leave `content` empty while filling `reasoning_content`.
/// A strict `as_str()` on `content` turned such healthy answers into
/// "Failed to parse" errors and benched the provider for an hour.
fn extract_message_content(json: &serde_json::Value) -> Option<String> {
    let msg = &json["choices"][0]["message"];
    if let Some(s) = msg["content"].as_str() {
        if !s.trim().is_empty() {
            return Some(s.to_string());
        }
    }
    if let Some(parts) = msg["content"].as_array() {
        let joined: String = parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("");
        if !joined.trim().is_empty() {
            return Some(joined);
        }
    }
    if let Some(s) = msg["reasoning_content"].as_str() {
        if !s.trim().is_empty() {
            return Some(s.to_string());
        }
    }
    None
}

fn truncate_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() > 250 {
        let mut truncated: String = trimmed.chars().take(250).collect();
        truncated.push_str("...");
        truncated
    } else {
        trimmed.to_string()
    }
}

pub fn clean_model_output(raw: &str) -> String {
    let mut text = raw.to_string();

    // Strip <think>...</think> (case-insensitive search)
    loop {
        let lower = text.to_lowercase();
        if let Some(start) = lower.find("<think>") {
            if let Some(end) = lower[start..].find("</think>") {
                let full_end = start + end + "</think>".len();
                text.replace_range(start..full_end, "");
            } else {
                // Unclosed think tag (e.g. max tokens hit during reasoning)
                text.truncate(start);
                break;
            }
        } else {
            break;
        }
    }

    // Strip [THINK]...[/THINK]
    loop {
        let lower = text.to_lowercase();
        if let Some(start) = lower.find("[think]") {
            if let Some(end) = lower[start..].find("[/think]") {
                let full_end = start + end + "[/think]".len();
                text.replace_range(start..full_end, "");
            } else {
                text.truncate(start);
                break;
            }
        } else {
            break;
        }
    }

    // Strip <reasoning>...</reasoning>
    loop {
        let lower = text.to_lowercase();
        if let Some(start) = lower.find("<reasoning>") {
            if let Some(end) = lower[start..].find("</reasoning>") {
                let full_end = start + end + "</reasoning>".len();
                text.replace_range(start..full_end, "");
            } else {
                text.truncate(start);
                break;
            }
        } else {
            break;
        }
    }

    text.trim().to_string()
}

async fn call_cloudflare_model(
    client: &reqwest::Client,
    model: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let account_id = std::env::var("CLOUDFLARE_ACCOUNT_ID").unwrap_or_default();
    let token = std::env::var("CLOUDFLARE_API_TOKEN").unwrap_or_default();
    if account_id.is_empty() || token.is_empty() {
        return Err(anyhow::anyhow!(
            "CLOUDFLARE_ACCOUNT_ID or CLOUDFLARE_API_TOKEN not configured"
        ));
    }
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 8192,
    });
    let url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/ai/v1/chat/completions",
        account_id
    );
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        let truncated = truncate_error_body(&body);
        return Err(anyhow::anyhow!(
            "Cloudflare {model} HTTP {status}: {truncated}"
        ));
    }

    let json = resp.json::<serde_json::Value>().await?;
    if let Some(choices) = json.get("choices") {
        if let Some(first) = choices.get(0) {
            if let Some(message) = first.get("message") {
                if let Some(content) = message.get("content") {
                    return content
                        .as_str()
                        .map(|s| s.to_string())
                        .ok_or_else(|| anyhow::anyhow!("Failed to parse Cloudflare response"));
                }
            }
        }
    }

    Err(anyhow::anyhow!(
        "Missing 'choices[0].message.content' in Cloudflare response"
    ))
}

async fn call_deepseek_model(model: &str, prompt: &str) -> anyhow::Result<String> {
    let key = std::env::var("DEEPSEEK_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Err(anyhow::anyhow!("DEEPSEEK_API_KEY not configured"));
    }

    // Serialize ONLY the short budget bookkeeping below — never the network
    // call. The old global lock was held for the whole request, so a DeepSeek
    // generator running 30-120s made the DeepSeek critic wait and time out on
    // every /ask (the logs showed "Critic deepseek-v4-flash timed out"), which
    // in turn degraded answers to the raw generator draft. DeepSeek has no
    // per-key concurrency limit, so parallel calls are safe.
    let budget = super::deepseek_budget::DeepSeekBudget::instance();
    let reserved = budget
        .reserve(prompt)
        .map_err(|e| anyhow::anyhow!("DeepSeek budget: {}", e))?;

    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 8192,
    });
    // Dedicated client: DeepSeek V4 Flash is a reasoning model and can exceed the
    // 60s pipeline timeout on long prompts — give it room so it doesn't get benched.
    let api_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .use_rustls_tls()
        .build()?;
    let resp = api_client
        .post("https://api.deepseek.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {key}"))
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "DeepSeek {model} HTTP {status}: {}",
            truncate_error_body(&body)
        ));
    }
    let json = resp.json::<serde_json::Value>().await?;

    // Commit actual usage (prefer API-reported totals).
    let used = json["usage"]["total_tokens"]
        .as_i64()
        .unwrap_or_else(|| (prompt.len() as i64 / 4).max(1));
    budget.settle(reserved, used.max(1));
    budget.record_daily(model, used.max(1));

    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Failed to parse DeepSeek response for {model}"))
}

pub async fn call_any_model_with_search(
    client: &reqwest::Client,
    gemini_key: &str,
    alias: &str,
    prompt: &str,
    db: Option<std::sync::Arc<crate::db::sqlite::SqliteStore>>,
    budget: Option<&super::search::BudgetManager>,
) -> anyhow::Result<String> {
    let mut current_prompt = prompt.to_string();

    if budget.is_some() {
        current_prompt.push_str(super::search::get_search_instructions());
    }

    let mut iterations = 0;
    let max_iterations = 5;

    loop {
        let response =
            call_any_model(client, gemini_key, alias, &current_prompt, db.clone()).await?;

        if let Some(b) = budget {
            if let Some(query) = super::search::parse_search_request(&response) {
                iterations += 1;
                if iterations > max_iterations {
                    tracing::warn!(
                        "Max search iterations ({}) reached for model {}. Exiting loop.",
                        max_iterations,
                        alias
                    );
                    return Ok(response);
                }

                let search_results = super::search::search_dispatch(client, b, &query, false).await;
                let formatted = super::search::format_search_results(&search_results);

                current_prompt.push_str(&format!("\nSEARCH: {}\n{}", query, formatted));
                continue;
            }
        }

        return Ok(response);
    }
}

// ── Live probe ───────────────────────────────────────────────────────────────

/// Outcome of one live provider probe. `status` is the HTTP status when a
/// response was received, `0` when the request never reached the provider
/// (missing key, timeout, local budget gate).
pub struct ProbeResult {
    pub provider: String,
    pub model: String,
    pub status: u16,
    pub ok: bool,
    pub latency_ms: i64,
    pub detail: String,
}

/// Probe every configured model with a tiny prompt.
///
/// Deliberately bypasses `call_single_model`: a probe must report what the
/// provider answers *right now*, not what the in-memory benches believe. That
/// makes it usable both as a health check and as a way to revive a model that
/// was benched by an unlucky 429 or a parse bug. Probe traffic is not written to
/// `model_stats` (it would skew the production error rate) and never benches a
/// model on failure; only successes clear stale bench state.
pub async fn probe_all_models(
    client: &reqwest::Client,
    gemini_key: &str,
    db: Option<std::sync::Arc<crate::db::sqlite::SqliteStore>>,
) -> Vec<ProbeResult> {
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    // 6 concurrent: enough to finish a 17-model sweep in ~3 waves, low enough
    // that per-minute rate limits are not tripped (which would fake a failure).
    let sem = Arc::new(Semaphore::new(6));
    let models = available_models();
    let mut handles = Vec::with_capacity(models.len());

    for info in models {
        let permit = sem.clone();
        let client = client.clone();
        let key = gemini_key.to_string();
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            let _guard = permit.acquire().await;
            probe_one(&client, &key, info, db).await
        }));
    }

    // Join in submission order so the table keeps the pool's priority order.
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        if let Ok(r) = h.await {
            out.push(r);
        }
    }

    // One line per sweep so an operator can confirm the button actually ran and
    // see what broke without opening the chat.
    let ok = out.iter().filter(|r| r.ok).count();
    tracing::info!("AI probe: {}/{} models answered", ok, out.len());
    for r in out.iter().filter(|r| !r.ok) {
        tracing::warn!(
            "AI probe failed: {}/{} HTTP {}: {}",
            r.provider,
            r.model,
            r.status,
            r.detail
        );
    }
    out
}

async fn probe_one(
    client: &reqwest::Client,
    gemini_key: &str,
    info: ModelInfo,
    db: Option<std::sync::Arc<crate::db::sqlite::SqliteStore>>,
) -> ProbeResult {
    let started = std::time::Instant::now();
    let result =
        dispatch_provider_call(client, gemini_key, &info, "Reply with the single word OK.").await;
    let latency_ms = started.elapsed().as_millis() as i64;
    let provider = format!("{:?}", info.provider);
    let provider_id = format!("{}/{}", provider, info.id);

    match result {
        Ok(_) => {
            // A real answer is proof the model works: drop a stale circuit
            // breaker, a provider-wide quota bench and a COOLDOWN/DEAD row.
            crate::crypto::ai::pools::clear_provider_model_ban(&info.provider, info.id.as_str());
            if let Some(store) = &db {
                store.update_model_health(&provider_id, "OK", None);
            }
            ProbeResult {
                provider,
                model: info.id,
                status: 200,
                ok: true,
                latency_ms,
                detail: String::new(),
            }
        }
        Err(e) => {
            let msg = e.to_string();
            ProbeResult {
                provider,
                model: info.id,
                status: extract_http_status(&msg),
                ok: false,
                latency_ms,
                detail: truncate_error_body(&msg),
            }
        }
    }
}

/// Pull `HTTP <code>` out of a provider error string. Errors that never got a
/// response (missing key, timeout, local budget gate) yield `0`.
fn extract_http_status(err: &str) -> u16 {
    if err.contains("timed out") || err.contains("timeout") {
        return 408;
    }
    if let Some(pos) = err.find("HTTP ") {
        let digits: String = err[pos + 5..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(code) = digits.parse::<u16>() {
            return code;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_content_handles_string_array_and_reasoning() {
        let as_string = serde_json::json!({
            "choices": [{"message": {"content": "hello"}}]
        });
        assert_eq!(
            extract_message_content(&as_string).as_deref(),
            Some("hello")
        );

        let as_chunks = serde_json::json!({
            "choices": [{"message": {"content": [
                {"type": "text", "text": "he"},
                {"type": "text", "text": "llo"}
            ]}}]
        });
        assert_eq!(
            extract_message_content(&as_chunks).as_deref(),
            Some("hello")
        );

        let reasoning_only = serde_json::json!({
            "choices": [{"message": {"content": "", "reasoning_content": "thought"}}]
        });
        assert_eq!(
            extract_message_content(&reasoning_only).as_deref(),
            Some("thought")
        );

        let empty = serde_json::json!({"choices": [{"message": {"content": "  "}}]});
        assert!(extract_message_content(&empty).is_none());
    }

    #[test]
    fn estimate_tokens_never_underestimates_large_prompts() {
        // 6000-token prompt ≈ 18k chars with the 3 chars/token heuristic.
        let prompt = "a".repeat(18_000);
        assert!(estimate_tokens(&prompt) >= 6000);
        assert!(estimate_tokens("") >= 1);
    }

    #[test]
    fn groq_skips_prompts_that_can_only_get_413() {
        // The on-demand tier caps at 8k TPM; an oversized prompt must fail
        // locally instead of burning an HTTP round-trip on a guaranteed 413.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = reqwest::Client::new();
        let big = "x".repeat(60_000);
        let err = rt
            .block_on(call_groq_model(&client, "openai/gpt-oss-120b", &big))
            .unwrap_err()
            .to_string();
        assert!(err.contains("prompt too large"), "got: {err}");
    }
}
