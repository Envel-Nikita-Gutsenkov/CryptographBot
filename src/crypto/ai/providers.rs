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
    let file_content = std::fs::read_to_string("models.json").unwrap_or_default();
    let models_file: crate::crypto::ai::pools::ModelsFile = serde_json::from_str(&file_content)
        .unwrap_or_else(|_| crate::crypto::ai::pools::ModelsFile { models: vec![] });
    models_file
        .models
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

    let result = match info.provider {
        Provider::Gemini => {
            let body = serde_json::json!({
                "contents": [{"parts": [{"text": prompt}]}]
            });
            call_gemini_model(client, gemini_key, info.id.as_str(), &body).await
        }
        Provider::Groq => call_groq_model(client, info.id.as_str(), prompt).await,
        Provider::Mistral => call_mistral_model(client, info.id.as_str(), prompt).await,
        Provider::Nvidia => call_nvidia_model(client, info.id.as_str(), prompt).await,
        Provider::OpenRouter => call_openrouter_model(client, info.id.as_str(), prompt).await,
        Provider::Cohere => call_cohere_model(client, info.id.as_str(), prompt).await,
        Provider::Cerebras => call_cerebras_model(client, info.id.as_str(), prompt).await,
        Provider::HuggingFace => call_huggingface_model(client, info.id.as_str(), prompt).await,
        Provider::Cloudflare => call_cloudflare_model(client, info.id.as_str(), prompt).await,
        Provider::DeepSeek => call_deepseek_model(info.id.as_str(), prompt).await,
    };

    match result {
        Ok(res) => {
            crate::crypto::ai::metrics::record_success(&provider_id);
            // Real success → mark healthy (clears any stale ban).
            if let Some(store) = &db {
                store.update_model_health(&provider_id, "OK", None);
            }
            Ok(res)
        }
        Err(e) => {
            tracing::warn!(
                "Provider {:?} failed for model {}: {}",
                info.provider,
                info.id,
                e
            );
            crate::crypto::ai::metrics::record_error(&provider_id);
            crate::crypto::ai::pools::ban_provider_model(&info.provider, info.id.as_str());
            // Real failure → temporarily bench the model (1h COOLDOWN in DB,
            // 60s in-memory circuit breaker). Health comes from real traffic only.
            if let Some(store) = &db {
                store.update_model_health(&provider_id, "COOLDOWN", Some(e.to_string()));
            }
            Err(e)
        }
    }
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

async fn call_groq_model(
    client: &reqwest::Client,
    model: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let key = std::env::var("GROQ_API_KEY").unwrap_or_default();
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
    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Failed to parse Groq response for {model}"))
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
    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Failed to parse Mistral response for {model}"))
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

async fn call_openrouter_model(
    client: &reqwest::Client,
    model: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Err(anyhow::anyhow!("OPENROUTER_API_KEY not configured"));
    }
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

    // Serialize DeepSeek calls so budget check + settle are race-free (hard cap,
    // DeepSeek has no per-key limits so we enforce them here).
    static CALL_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = CALL_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;

    let budget = super::deepseek_budget::DeepSeekBudget::instance();
    budget
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
    budget.settle(used.max(1));
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
