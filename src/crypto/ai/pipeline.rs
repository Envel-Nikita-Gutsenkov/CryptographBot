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

    // 1. Race Dynamic Generators (bounded — a stuck provider must never hang /ask)
    let (draft_a, draft_b, gen_a, gen_b) = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        race_dynamic_generators(
            client.clone(),
            gemini_key.to_string(),
            prompt.to_string(),
            preferred_model.map(|s| s.to_string()),
            db.clone(),
            budget_mgr.clone(),
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Generator race timed out after 120s"))??;

    // 2. Build Critic Prompt
    let format_instruction = if is_json {
        "You MUST output valid JSON ONLY, matching the requested format."
    } else {
        "You MUST output the final text preserving the exact HTML formatting requested."
    };
    let critic_prompt = build_moe_critic_prompt(prompt, &draft_a, &draft_b, format_instruction);

    // 3. Run Critic Queue (bounded — same hang protection as the generators)
    let (critic_model, critic_res) = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        run_dynamic_critic(
            client.clone(),
            gemini_key.to_string(),
            critic_prompt,
            preferred_model.map(|s| s.to_string()),
            db,
            budget_mgr,
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Critic queue timed out after 120s"))??;

    if is_json {
        let clean = super::providers::clean_model_output(&critic_res);
        let start_idx = clean.find('{').unwrap_or(0);
        let end_idx = clean.rfind('}').map(|i| i + 1).unwrap_or(clean.len());
        let clean_json = if start_idx < end_idx {
            &clean[start_idx..end_idx]
        } else {
            clean.trim()
        };
        return Ok(clean_json.to_string());
    } else {
        let clean_critic = super::providers::clean_model_output(&critic_res);
        let mut final_text = String::new();
        let mut critic_succeeded = false;

        if let Some(start_pos) = clean_critic.find("<final_analysis>") {
            let content_start = start_pos + "<final_analysis>".len();
            if let Some(end_pos) = clean_critic[content_start..].find("</final_analysis>") {
                let candidate = clean_critic[content_start..content_start + end_pos].trim();
                if !candidate.is_empty() && !is_meta_critique(candidate) {
                    final_text = candidate.to_string();
                    critic_succeeded = true;
                }
            } else {
                let candidate = clean_critic[content_start..].trim();
                if !candidate.is_empty() && !is_meta_critique(candidate) {
                    final_text = candidate.to_string();
                    critic_succeeded = true;
                }
            }
        }

        if !critic_succeeded {
            let trimmed = clean_critic.trim();
            if !trimmed.is_empty() && !is_meta_critique(trimmed) {
                final_text = trimmed.to_string();
                critic_succeeded = true;
            }
        }

        // Safe fallback if critic output was a meta-critique, empty, or malformed.
        // NEVER leak a raw chain-of-thought to the user: if every candidate is
        // meta-commentary, fail loudly instead of sending the reasoning text.
        if !critic_succeeded {
            tracing::warn!(
                "Critic output was meta-critique or malformed. Falling back to generator draft."
            );
            let clean_a = super::providers::clean_model_output(&draft_a);
            let clean_b = super::providers::clean_model_output(&draft_b);
            let usable_a = !clean_a.trim().is_empty() && !is_meta_critique(&clean_a);
            let usable_b = !clean_b.trim().is_empty() && !is_meta_critique(&clean_b);
            if usable_a {
                final_text = clean_a.trim().to_string();
            } else if usable_b {
                final_text = clean_b.trim().to_string();
            } else {
                return Err(anyhow::anyhow!(
                    "All model outputs were meta-commentary or empty — refusing to send raw reasoning. Please try again."
                ));
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
            let critic_label = if critic_succeeded {
                format!(" → Critic: {}", fmt(&critic_model))
            } else {
                String::new()
            };
            return Ok(format!("{final_text}\n\n_Gen: {gen_label}{critic_label}_"));
        } else {
            return Ok(final_text);
        }
    }
}

pub fn is_meta_critique(text: &str) -> bool {
    let lower = text.to_lowercase();
    let meta_indicators = [
        "draft a",
        "draft b",
        "draft 1",
        "draft 2",
        "looking at draft",
        "evaluating both drafts",
        "evaluating the drafts",
        "comparing drafts",
        "checking for hallucinations",
        "check for hallucinations",
        "in terms of hallucinations",
        "the user wants me to evaluate",
        "the user is asking me to evaluate",
        "okay, let's tackle this",
        "let me start by",
        "i'll start by",
        "i will start by",
        "first, let me",
        "let me analyze",
        "let me think",
        "let me consider",
        "let me check",
        "let me review",
        "let me look at",
        "let me break down",
        "let's break this down",
        "i need to analyze",
        "i need to check",
        "i need to verify",
        "i need to evaluate",
        "let's analyze the request",
        "хорошо, я должен проанализировать",
        "я должен проанализировать",
        "я должен проверить",
        "я должен оценить",
        "мне нужно проанализировать",
        "мне нужно проверить",
        "мне нужно оценить",
        "начну с проверки",
        "я должен оценить два черновика",
        "черновик а",
        "черновик б",
        "черновика а",
        "черновика б",
        "оцениваю черновики",
        "проверка черновиков",
    ];

    for ind in &meta_indicators {
        if lower.contains(ind) {
            return true;
        }
    }

    false
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
                let res = tokio::time::timeout(
                    std::time::Duration::from_secs(60),
                    super::providers::call_any_model_with_search(
                        &c,
                        &k,
                        &model,
                        &p,
                        Some(db2),
                        Some(&budget2),
                    ),
                )
                .await
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!(
                        "Generator {} timed out after 60s",
                        model_clone
                    ))
                });
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
                        let res = tokio::time::timeout(
                            std::time::Duration::from_secs(60),
                            super::providers::call_any_model_with_search(
                                &c,
                                &k,
                                &next_model,
                                &p,
                                Some(db3),
                                Some(&budget3),
                            ),
                        )
                        .await
                        .unwrap_or_else(|_| {
                            Err(anyhow::anyhow!(
                                "Generator {} timed out after 60s",
                                next_model_clone
                            ))
                        });
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

    // Last resort: try several independent providers so a single outage (e.g.
    // Gemini quota) can't hard-fail the pipeline when the pool is exhausted.
    tracing::warn!("All generators failed. Trying last-resort fallback...");
    match super::providers::call_last_resort(&client, &gemini_key, &prompt).await {
        Ok((label, res)) => {
            super::pools::record_successful_model(&label);
            return Ok((
                res.clone(),
                String::from("(single fallback draft)"),
                label.clone(),
                label,
            ));
        }
        Err(e) => tracing::warn!("Last-resort fallback failed: {}", e),
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
        match tokio::time::timeout(
            std::time::Duration::from_secs(25),
            super::providers::call_any_model_with_search(
                &client,
                &gemini_key,
                &critic,
                &critic_prompt,
                Some(db.clone()),
                Some(&budget),
            ),
        )
        .await
        {
            Ok(Ok(res)) => {
                tracing::info!("Critic {} finished successfully!", critic);
                if std::env::var("CRYPTOGRAPH_DEBUG_PROMPTS").unwrap_or_default() == "1" {
                    println!("\n=== CRITIC {critic} OUTPUT ===\n{res}\n=== END CRITIC ===");
                }
                super::pools::record_successful_model(&critic);
                return Ok((critic, res));
            }
            Ok(Err(e)) => {
                tracing::warn!("Critic {} failed: {}", critic, e);
                if std::env::var("CRYPTOGRAPH_DEBUG_PROMPTS").unwrap_or_default() == "1" {
                    println!("   ❌ [FAILED] Critic {}: {}", critic, e);
                }
                last_err = e;
                continue;
            }
            Err(_) => {
                tracing::warn!("Critic {} timed out after 25s", critic);
                last_err = anyhow::anyhow!("Critic {} timed out after 25s", critic);
                continue;
            }
        }
    }

    // Last resort: try several independent providers so a single outage (e.g.
    // Gemini quota) can't hard-fail the critic stage when the pool is exhausted.
    tracing::warn!("All critics failed. Trying last-resort fallback...");
    match super::providers::call_last_resort(&client, &gemini_key, &critic_prompt).await {
        Ok((label, res)) => {
            super::pools::record_successful_model(&label);
            return Ok((label, res));
        }
        Err(e) => tracing::warn!("Last-resort critic fallback failed: {}", e),
    }

    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::ai::providers::clean_model_output;

    #[test]
    fn test_is_meta_critique_detection() {
        assert!(is_meta_critique("Okay, let's tackle this. The user wants me to evaluate two drafts based on the original request... Looking at Draft A: ... Draft B: ... Now, checking for hallucinations."));
        assert!(is_meta_critique("Хорошо, я должен проанализировать вопрос пользователя о том, когда криптовалюты вернутся..."));
        assert!(is_meta_critique(
            "In evaluating both drafts, Draft A has better numbers."
        ));
        assert!(!is_meta_critique("Bitcoin is currently trading at 64,000 USD. Technical indicators suggest strong support at 62,000 USD."));
    }

    #[test]
    fn test_is_meta_critique_catches_leaked_cot() {
        // Exact patterns observed leaking to the user on 2026-08-29 (Cohere
        // reasoning models emitting their chain-of-thought as the answer).
        assert!(is_meta_critique(
            "Okay, let me start by carefully reviewing the user's question and the provided data. The user is asking when they can expect their crypto holdings to return to their average purchase prices."
        ));
        assert!(is_meta_critique(
            "Хорошо, я должен проанализировать вопрос пользователя о том, когда криптовалюты вернутся к его средним ценам покупки и произойдет ли это в этом году. Начну с проверки предоставленных данных."
        ));
        assert!(is_meta_critique(
            "I'll start by reviewing the portfolio data and checking the current market conditions."
        ));
        assert!(is_meta_critique(
            "Let me analyze the user's question about when BTC might return to 60-63k."
        ));
        assert!(is_meta_critique(
            "Мне нужно проверить текущие цены и сравнить их со средними закупочными."
        ));
    }

    #[test]
    fn test_is_meta_critique_does_not_flag_real_answers() {
        // Real answers from the chat log must NOT be flagged.
        assert!(!is_meta_critique(
            "Здорово, братан! Вопрос отличный и очень своевременный. Давай снимем розовые очки, посмотрим на сухие цифры рынка на сегодня и разберем, реально ли увидеть твои средние закупочные цены до конца этого года."
        ));
        assert!(!is_meta_critique(
            "Понимаю ваше беспокойство — цены действительно выросли. Давайте разберёмся, почему был дан тот сигнал и как действовать дальше."
        ));
        assert!(!is_meta_critique(
            "BTC is trading at 78,000 USD, up 23% over 7 days. Support sits at 70,000 USD."
        ));
    }

    #[test]
    fn test_clean_model_output_stripping() {
        let think_sample = "<think>Let me evaluate this carefully...\nDraft A is okay.</think>Ethereum dropped 5% over 7 days.";
        assert_eq!(
            clean_model_output(think_sample),
            "Ethereum dropped 5% over 7 days."
        );

        let unclosed_think = "<think>Still thinking and cut off...";
        assert_eq!(clean_model_output(unclosed_think), "");

        let clean_text = "Standard response without tags.";
        assert_eq!(
            clean_model_output(clean_text),
            "Standard response without tags."
        );
    }
}
