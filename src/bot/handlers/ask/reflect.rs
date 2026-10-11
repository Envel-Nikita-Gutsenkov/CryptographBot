//! Agentic reflection for `/ask`: after the first answer, a cheap watcher-tier
//! reviewer decides whether a specific fresh fact is still missing. If so, ONE
//! extra targeted search (or a local-knowledge pull) runs and the answer is
//! regenerated — bounded to a single iteration so /ask never spins.

use super::search::fetch_and_summarize;

/// Phase D — agentic reflection pass.
///
/// After the first answer is produced, a cheap watcher-tier reviewer decides
/// whether a specific piece of fresh information is still missing (current
/// prices, exact dates, what happened today, a forecast for a named month).
/// If a gap is found, we run ONE extra targeted web search, fetch+summarize
/// the top pages, and regenerate the answer with the new facts — bounded to a
/// single iteration so /ask never spins. Returns None when the answer is
/// already sufficient or any step fails (the first answer is kept).
pub(crate) async fn reflect_and_refine(
    client: &reqwest::Client,
    gemini_key: &str,
    question: &str,
    detected: &std::collections::HashSet<String>,
    original_prompt: &str,
    first_answer: &str,
    progress_tx: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    local_only: bool,
    preferred_model: Option<&str>,
    sqlite: std::sync::Arc<crate::db::SqliteStore>,
) -> Option<String> {
    let models = crate::crypto::ai::pools::get_available_models(Some("watcher"));
    let coins = if detected.is_empty() {
        "none".to_string()
    } else {
        let mut v: Vec<&str> = detected.iter().map(|s| s.as_str()).collect();
        v.sort();
        v.join(", ")
    };
    let review_prompt = format!(
        "You are a quality-control reviewer for a crypto assistant.\n\
         Today's date: {}.\n\
         Coins the user cares about: {}.\n\
         The assistant already had: live prices, portfolio, news summaries, market intelligence \
         and possibly web search results.\n\
         USER QUESTION: \"{}\n\
         ASSISTANT'S ANSWER:\n\"{}\n\n\
         Decide whether the answer is missing a SPECIFIC piece of fresh, verifiable information \
         the user clearly asked for (e.g. current price levels, exact dates of upcoming events, \
         what happened today, a forecast for a named month).\n\
         If the gap can be closed from LOCAL data the assistant already stores (price charts, \
         indicators, news summary history for a coin), output EXACTLY one line:\n\
         LOCAL: <SYMBOL> <DAYS>  (e.g. \"LOCAL: SOL 30\", days between 7 and 365)\n\
         Otherwise, if a web search is needed, output EXACTLY one line: REFINE: <one concise English search query>\n\
         The query must be in ENGLISH, must NOT copy the user's wording, and must target the \
         missing fact (e.g. \"BTC price forecast September 2026\", \"CLARITY Act vote date\").\n\
         If the answer is already sufficient, output exactly: SUFFICIENT",
        chrono::Utc::now().format("%Y-%m-%d"),
        coins,
        question,
        first_answer
    );

    let mut refine_query: Option<String> = None;
    let mut local_request: Option<(String, u32)> = None;
    for alias in models.into_iter().take(2) {
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::crypto::ai::providers::call_any_model(
                client,
                gemini_key,
                &alias,
                &review_prompt,
                None,
            ),
        )
        .await;
        match resp {
            Ok(Ok(text)) => {
                let lower = text.to_lowercase();
                if lower.contains("sufficient") {
                    return None;
                }
                for line in text.lines() {
                    let t = line.trim();
                    let t_lower = t.to_lowercase();
                    if t_lower.starts_with("refine:") {
                        let q = t[7..].trim().trim_matches('"').trim();
                        // Drop echoes of the user's wording (non-ASCII) and
                        // empty queries — same guard as the search planner.
                        if !q.is_empty() && q.chars().all(|c| c.is_ascii() || c == ' ') {
                            refine_query = Some(q.to_string());
                            break;
                        }
                    } else if t_lower.starts_with("local:") || t_lower.starts_with("news:") {
                        // LOCAL: SYMBOL DAYS — pull pre-digested local
                        // knowledge (chart indicators + news summary history)
                        // instead of burning tokens on raw pages.
                        let colon = t.find(':').unwrap_or(0);
                        let rest = t[colon + 1..].trim();
                        let mut parts = rest.split_whitespace();
                        if let (Some(sym), Some(days)) = (parts.next(), parts.next()) {
                            if let Ok(d) = days.parse::<u32>() {
                                if (7..=365).contains(&d) {
                                    local_request = Some((sym.to_uppercase(), d));
                                    break;
                                }
                            }
                        }
                    }
                }
                if refine_query.is_some() || local_request.is_some() {
                    break;
                }
                // Unusable output — try the next reviewer model.
            }
            _ => continue,
        }
    }

    // LOCAL knowledge request — cheaper than a web search and uses the
    // pre-digested summaries the bot already stores.
    if let Some((sym, days)) = local_request {
        if let Some(tx) = progress_tx {
            let _ = tx.send(format!(
                "\u{1f9e0} Fetching local history for {sym} ({days}d)..."
            ));
        }
        let mut local_block = String::new();
        let chart =
            crate::crypto::ai::market_memory::chart_context_block(&sqlite, &[sym.clone()], days)
                .await;
        if !chart.is_empty() {
            local_block.push_str(&chart);
        }
        let calendar =
            crate::crypto::ai::market_memory::seasonality_context_block(&[sym.clone()]).await;
        if !calendar.is_empty() {
            local_block.push_str(&format!("\n\n{}", calendar));
        }
        let locality =
            crate::crypto::ai::market_memory::locality_context_block(&[sym.clone()]).await;
        if !locality.is_empty() {
            local_block.push_str(&format!("\n\n{}", locality));
        }
        let regime_block =
            crate::crypto::ai::market_memory::regime_context_block(&[sym.clone()]).await;
        if !regime_block.is_empty() {
            local_block.push_str(&format!("\n\n{}", regime_block));
        }
        if let Ok(rows) = sqlite.list_news_summary_history(&sym, 5).await {
            if !rows.is_empty() {
                let mut news_lines = Vec::new();
                for (summary, ts) in rows.iter().take(5) {
                    let head: String = summary.chars().take(400).collect();
                    news_lines.push(format!("[{ts}] {head}"));
                }
                local_block.push_str(&format!(
                    "\n\nNEWS SUMMARY HISTORY ({sym}, last {} days):\n{}",
                    days,
                    news_lines.join("\n")
                ));
            }
        }
        if local_block.trim().is_empty() {
            return None;
        }
        let refine_prompt = format!(
            "{}\n\n\
             === REFINEMENT PASS (agentic self-review) ===\n\
             A first draft answer was already produced, then a self-review found a gap and the \
             assistant fetched LOCAL pre-digested data (chart indicators + news summary history). \
             Improve the draft: add the missing concrete facts (price levels, trends, news \
             timeline), keep the existing structure and tone. Output the final improved answer only.\n\n\
             FIRST DRAFT ANSWER:\n\"{}\n\n\
             LOCAL KNOWLEDGE (fetched after reflection):\n{}",
            original_prompt, first_answer, local_block
        );
        return tokio::time::timeout(
            std::time::Duration::from_secs(120),
            crate::crypto::ai::pipeline::call_with_critic(
                client,
                gemini_key,
                &refine_prompt,
                preferred_model,
                false, // is_json = false
                true,  // append_footer = true
                sqlite,
            ),
        )
        .await
        .ok()
        .and_then(|r| r.ok());
    }

    let query = refine_query?;

    if let Some(tx) = progress_tx {
        let _ = tx.send(format!("\u{1f9e0} Found a gap — searching: {}", query));
    }

    let budget_mgr = crate::crypto::ai::search::BudgetManager::new(
        crate::crypto::ai::search::SearchBudget::new(),
    );
    let results =
        crate::crypto::ai::search::search_dispatch(client, &budget_mgr, &query, local_only).await;
    if results.is_empty() {
        return None;
    }
    let digest = fetch_and_summarize(
        client,
        gemini_key,
        question,
        &results,
        progress_tx,
        local_only,
    )
    .await;
    if digest.trim().is_empty() {
        return None;
    }

    let refine_prompt = format!(
        "{}\n\n\
         === REFINEMENT PASS (agentic self-review) ===\n\
         A first draft answer was already produced, then a self-review found a gap and new \
         research was fetched. Improve the draft: correct anything the new research contradicts, \
         add the missing concrete facts (prices, dates, events), keep the existing structure and \
         tone. Output the final improved answer only.\n\n\
         FIRST DRAFT ANSWER:\n\"{}\n\n\
         ADDITIONAL RESEARCH (fetched after reflection):\n{}",
        original_prompt, first_answer, digest
    );

    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        crate::crypto::ai::pipeline::call_with_critic(
            client,
            gemini_key,
            &refine_prompt,
            preferred_model,
            false, // is_json = false
            true,  // append_footer = true
            sqlite,
        ),
    )
    .await
    .ok()
    .and_then(|r| r.ok())
}

/// Questions that benefit from the agentic reflection pass: forecasts, "when",
/// "what happened", news and analysis questions — the ones where a vague
/// single-pass answer is most likely. Simple factual questions skip it (the
/// cheap reviewer call is only worth it when a gap is plausible).
pub(crate) fn needs_reflection(question_lower: &str) -> bool {
    const TRIGGERS: &[&str] = &[
        // ru
        "когда",
        "будет",
        "ждем",
        "ждёт",
        "ждет",
        "прогноз",
        "ожид",
        "вероятн",
        "анализ",
        "случил",
        "произошл",
        "сегодня",
        "стоит ли",
        "какой сейчас",
        "какая сейчас",
        "актуал",
        "свеж",
        "последн",
        "рекоменд",
        "новост",
        "цена",
        "цене",
        "цену",
        "откат",
        "упадет",
        "упадёт",
        "вырастет",
        "выраст",
        // en
        "when",
        "will",
        "forecast",
        "predict",
        "expect",
        "probability",
        "analysis",
        "happen",
        "today",
        "current",
        "latest",
        "news",
        "price",
        "should i",
        "drop",
        "fall",
        "rally",
    ];
    TRIGGERS.iter().any(|t| question_lower.contains(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflection_gate_fires_for_forecast_and_news() {
        assert!(needs_reflection("когда ждем крипту по 60-63к"));
        assert!(needs_reflection("что произошло сегодня с эфиром"));
        assert!(needs_reflection("BTC price forecast 2026"));
        assert!(needs_reflection("стоит ли покупать сейчас"));
        assert!(!needs_reflection("покажи мой портфель"));
        assert!(!needs_reflection("как добавить покупку"));
    }
}
