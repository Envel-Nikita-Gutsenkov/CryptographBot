use super::prompts::{build_moe_critic_prompt, build_news_judge_prompt};
use futures::stream::{FuturesUnordered, StreamExt};

#[allow(dead_code)]
pub async fn generate_moe_pipeline(
    prompt_base: &str,
    previous_summary: Option<&str>,
    db: std::sync::Arc<crate::db::sqlite::SqliteStore>,
) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .use_rustls_tls()
        .build()?;
    let gemini_key = std::env::var("GEMINI_API_KEY").unwrap_or_default();

    let search_budget = super::search::SearchBudget::new();
    let budget_mgr = std::sync::Arc::new(super::search::BudgetManager::new(search_budget));

    // 1. Race Dynamic Generators
    let (draft_a, draft_b, _gen_a, _gen_b) = race_dynamic_generators(
        client.clone(),
        gemini_key.clone(),
        prompt_base.to_string(),
        None,
        db.clone(),
        budget_mgr.clone(),
    )
    .await?;

    // 2. Build Critic Prompt
    let prev_context = previous_summary
        .map(|s| format!("PREVIOUS 2H SUMMARY:\n{s}\n(CRITICAL: Do not wildly contradict this unless new evidence explicitly proves otherwise.)\n"))
        .unwrap_or_default();
    let critic_prompt = build_news_judge_prompt(
        prompt_base,
        &prev_context,
        "Generator-A",
        &draft_a,
        "Generator-B",
        &draft_b,
    );

    // 3. Run Critic Queue
    let (_, res) =
        run_dynamic_critic(client, gemini_key, critic_prompt, None, db, budget_mgr).await?;
    Ok(res)
}

pub async fn summarize_news_single_pass(
    symbol: &str,
    news_text: &str,
    market_data: &str,
    previous_summary: Option<&str>,
    db: std::sync::Arc<crate::db::sqlite::SqliteStore>,
) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .use_rustls_tls()
        .build()?;
    let gemini_key = std::env::var("GEMINI_API_KEY").unwrap_or_default();

    let prev_context = previous_summary
        .map(|s| {
            format!(
                "PREVIOUS SUMMARY:
{s}
(Do not contradict this wildly unless new evidence proves otherwise.)
"
            )
        })
        .unwrap_or_default();

    let prompt = format!(
        "You are an expert cryptocurrency analyst acting as the Chief Editor for {symbol}.\n\n\
        Here is the current market status:\n\
        {market_data}\n\n\
        {prev_context}\n\n\
        Your task is to analyze the following latest news and summarize them into a cohesive, highly professional, and dense market summary. Focus on macro narrative, sentiment, and critical price drivers. Use bullet points. DO NOT output fluff.\n\n\
        LATEST NEWS:\n\
        {news_text}\n\n\
        Provide the final summary now:"
    );

    let mut map_models: Vec<String> = super::pools::get_available_models(Some("news"));

    // Always append gemini-2.5-flash as the last resort fallback
    if !map_models.contains(&"gemini-2.5-flash".to_string()) {
        map_models.push("gemini-2.5-flash".to_string());
    }

    if std::env::var("CRYPTOGRAPH_DEBUG_PROMPTS").unwrap_or_default() == "1" {
        println!(
            "\n=== [AI NEWS PROMPT] ===\n{}\n=== [END PROMPT] ===\n",
            prompt
        );
    }

    let search_budget = super::search::SearchBudget::new();
    let budget_mgr = super::search::BudgetManager::new(search_budget);

    let mut last_err = anyhow::anyhow!("No models available for news mapping");
    for map_model in map_models {
        match super::providers::call_any_model_with_search(
            &client,
            &gemini_key,
            &map_model,
            &prompt,
            Some(db.clone()),
            Some(&budget_mgr),
        )
        .await
        {
            Ok(res) => {
                let cleaned = res
                    .lines()
                    .filter(|l| {
                        let tr = l.trim().to_lowercase();
                        !tr.contains("chief editor")
                            && !tr.contains("professional, dense")
                            && !tr.contains("current market status")
                            && !tr.contains("macro narrative")
                            && !tr.contains("section:")
                            && !tr.contains("section ")
                            && !tr.contains("refining the")
                            && !tr.contains("critical price drivers")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                return Ok(cleaned.trim().to_string());
            }
            Err(e) => {
                tracing::warn!("Model {} failed for news: {}", map_model, e);
                if std::env::var("CRYPTOGRAPH_DEBUG_PROMPTS").unwrap_or_default() == "1" {
                    println!("   ❌ [FAILED] {}: {}", map_model, e);
                }
                last_err = e;
            }
        }
    }

    Err(last_err)
}

pub async fn call_with_critic(
    client: &reqwest::Client,
    gemini_key: &str,
    prompt: &str,
    preferred_model: Option<&str>,
    is_json: bool,
    append_footer: bool,
    db: std::sync::Arc<crate::db::sqlite::SqliteStore>,
) -> anyhow::Result<String> {
    if std::env::var("CRYPTOGRAPH_DEBUG_PROMPTS").unwrap_or_default() == "1" {
        println!(
            "\n=== [AI SYSTEM PROMPT] ===\n{}\n=== [END PROMPT] ===\n",
            prompt
        );
    }

    let search_budget = super::search::SearchBudget::new();
    let budget_mgr = std::sync::Arc::new(super::search::BudgetManager::new(search_budget));

    // 1. Race Dynamic Generators
    let (draft_a, draft_b, gen_a, gen_b) = race_dynamic_generators(
        client.clone(),
        gemini_key.to_string(),
        prompt.to_string(),
        preferred_model.map(|s| s.to_string()),
        db.clone(),
        budget_mgr.clone(),
    )
    .await?;

    // 2. Build Critic Prompt
    let format_instruction = if is_json {
        "You MUST output valid JSON ONLY, matching the requested format."
    } else {
        "You MUST output the final text preserving the exact HTML formatting requested."
    };
    let critic_prompt = build_moe_critic_prompt(prompt, &draft_a, &draft_b, format_instruction);

    // 3. Run Critic Queue
    let (critic_model, critic_res) = run_dynamic_critic(
        client.clone(),
        gemini_key.to_string(),
        critic_prompt,
        preferred_model.map(|s| s.to_string()),
        db,
        budget_mgr,
    )
    .await?;

    if is_json {
        let start_idx = critic_res.find('{').unwrap_or(0);
        let end_idx = critic_res
            .rfind('}')
            .map(|i| i + 1)
            .unwrap_or(critic_res.len());
        let clean_json = if start_idx < end_idx {
            &critic_res[start_idx..end_idx]
        } else {
            critic_res.trim()
        };
        return Ok(clean_json.to_string());
    } else {
        let mut final_text = critic_res.trim();
        if let Some(start_pos) = final_text.find("<final_analysis>") {
            let content_start = start_pos + "<final_analysis>".len();
            if let Some(end_pos) = final_text[content_start..].find("</final_analysis>") {
                final_text = final_text[content_start..content_start + end_pos].trim();
            } else {
                final_text = final_text[content_start..].trim();
            }
        }
        if append_footer {
            let fmt = |alias: &str| -> String {
                let provs = super::pools::providers_for_alias(alias);
                if provs.is_empty() {
                    alias.to_string()
                } else {
                    format!("{} ({})", alias, provs.join("/"))
                }
            };
            let gen_label = if gen_a == gen_b {
                fmt(&gen_a)
            } else {
                format!("{} + {}", fmt(&gen_a), fmt(&gen_b))
            };
            return Ok(format!(
                "{final_text}\n\n_Gen: {gen_label} → Critic: {}_",
                fmt(&critic_model)
            ));
        } else {
            return Ok(final_text.to_string());
        }
    }
}

pub async fn race_dynamic_generators(
    client: reqwest::Client,
    gemini_key: String,
    prompt: String,
    preferred_model: Option<String>,
    db: std::sync::Arc<crate::db::sqlite::SqliteStore>,
    budget: std::sync::Arc<super::search::BudgetManager>,
) -> anyhow::Result<(String, String, String, String)> {
    let mut gens = super::pools::get_available_models(Some("main"));

    if let Some(pref) = preferred_model {
        if let Some(idx) = gens.iter().position(|m| m == pref.as_str()) {
            let p = gens.remove(idx);
            gens.insert(0, p);
        }
    }

    let mut tasks = FuturesUnordered::new();
    let mut successful: Vec<(String, String)> = Vec::new();

    for _ in 0..2 {
        if !gens.is_empty() {
            let model = gens.remove(0);
            let c = client.clone();
            let k = gemini_key.clone();
            let p = prompt.clone();
            let db2 = db.clone();
            let budget2 = budget.clone();
            tasks.push(tokio::spawn(async move {
                let model_clone = model.clone();
                let res = super::providers::call_any_model_with_search(
                    &c,
                    &k,
                    &model,
                    &p,
                    Some(db2),
                    Some(&budget2),
                )
                .await;
                (model_clone, res)
            }));
        }
    }

    while let Some(res) = tasks.next().await {
        match res {
            Ok((model, Ok(draft))) => {
                tracing::info!("Generator {} finished successfully!", model);
                if std::env::var("CRYPTOGRAPH_DEBUG_PROMPTS").unwrap_or_default() == "1" {
                    println!("\n=== GENERATOR {model} DRAFT ===\n{draft}\n=== END DRAFT ===");
                }
                super::pools::record_successful_model(&model);
                successful.push((model, draft));
                if successful.len() >= 2 {
                    let (m0, d0) = successful.remove(0);
                    let (m1, d1) = successful.remove(0);
                    return Ok((d0, d1, m0, m1));
                }
            }
            Ok((model, Err(e))) => {
                tracing::warn!("Generator {} failed: {}", model, e);
                if std::env::var("CRYPTOGRAPH_DEBUG_PROMPTS").unwrap_or_default() == "1" {
                    println!("   ❌ [FAILED] Generator {}: {}", model, e);
                }
                if !gens.is_empty() {
                    let next_model = gens.remove(0);
                    let c = client.clone();
                    let k = gemini_key.clone();
                    let p = prompt.clone();
                    let db3 = db.clone();
                    let budget3 = budget.clone();
                    tasks.push(tokio::spawn(async move {
                        let next_model_clone = next_model.clone();
                        let res = super::providers::call_any_model_with_search(
                            &c,
                            &k,
                            &next_model,
                            &p,
                            Some(db3),
                            Some(&budget3),
                        )
                        .await;
                        (next_model_clone, res)
                    }));
                }
            }
            Err(e) => {
                tracing::warn!("Task panic: {}", e);
            }
        }
    }

    if successful.len() == 1 {
        tracing::warn!("Only got 1 successful draft out of all generators.");
        let (m0, d0) = successful.remove(0);
        return Ok((
            d0,
            String::from("No second draft available (all other models failed)."),
            m0.clone(),
            m0,
        ));
    }

    // Last resort: direct, unfiltered Gemini call so the pipeline degrades gracefully
    // instead of hard-failing when health marks / rate limits exhaust the model pool.
    tracing::warn!("All generators failed. Trying last-resort fallback gemini-2.5-flash...");
    let fallback_body = serde_json::json!({
        "contents": [{"parts": [{"text": prompt}]}]
    });
    match super::providers::call_gemini_model(
        &client,
        &gemini_key,
        "gemini-2.5-flash",
        &fallback_body,
    )
    .await
    {
        Ok(res) => {
            super::pools::record_successful_model("gemini-2.5-flash");
            return Ok((
                res.clone(),
                String::from("(single fallback draft)"),
                "gemini-2.5-flash".to_string(),
                "gemini-2.5-flash".to_string(),
            ));
        }
        Err(e) => tracing::warn!("Last-resort fallback gemini-2.5-flash failed: {}", e),
    }

    Err(anyhow::anyhow!("Failed to generate any successful drafts"))
}

pub async fn run_dynamic_critic(
    client: reqwest::Client,
    gemini_key: String,
    critic_prompt: String,
    preferred_model: Option<String>,
    db: std::sync::Arc<crate::db::sqlite::SqliteStore>,
    budget: std::sync::Arc<super::search::BudgetManager>,
) -> anyhow::Result<(String, String)> {
    let mut critics = super::pools::get_available_models(Some("critic"));

    if let Some(pref) = preferred_model {
        if let Some(idx) = critics.iter().position(|m| m == pref.as_str()) {
            let p = critics.remove(idx);
            critics.insert(0, p);
        }
    }

    let mut last_err = anyhow::anyhow!("No critics available");
    for critic in critics {
        tracing::info!("Trying critic: {}", critic);
        if std::env::var("CRYPTOGRAPH_DEBUG_PROMPTS").unwrap_or_default() == "1" {
            println!(">> [AI Model Call] Critic {}", critic);
        }
        match super::providers::call_any_model_with_search(
            &client,
            &gemini_key,
            &critic,
            &critic_prompt,
            Some(db.clone()),
            Some(&budget),
        )
        .await
        {
            Ok(res) => {
                tracing::info!("Critic {} finished successfully!", critic);
                if std::env::var("CRYPTOGRAPH_DEBUG_PROMPTS").unwrap_or_default() == "1" {
                    println!("\n=== CRITIC {critic} OUTPUT ===\n{res}\n=== END CRITIC ===");
                }
                super::pools::record_successful_model(&critic);
                return Ok((critic, res));
            }
            Err(e) => {
                tracing::warn!("Critic {} failed: {}", critic, e);
                if std::env::var("CRYPTOGRAPH_DEBUG_PROMPTS").unwrap_or_default() == "1" {
                    println!("   ❌ [FAILED] Critic {}: {}", critic, e);
                }
                last_err = e;
                continue;
            }
        }
    }

    // Last resort: direct, unfiltered Gemini call so the critic stage degrades
    // gracefully instead of hard-failing when the whole pool is dead/skipped.
    tracing::warn!("All critics failed. Trying last-resort fallback gemini-2.5-flash...");
    let fallback_body = serde_json::json!({
        "contents": [{"parts": [{"text": critic_prompt}]}]
    });
    match super::providers::call_gemini_model(
        &client,
        &gemini_key,
        "gemini-2.5-flash",
        &fallback_body,
    )
    .await
    {
        Ok(res) => {
            super::pools::record_successful_model("gemini-2.5-flash");
            return Ok(("gemini-2.5-flash".to_string(), res));
        }
        Err(e) => tracing::warn!("Last-resort critic gemini-2.5-flash failed: {}", e),
    }

    Err(last_err)
}
