use super::types::SearchResult;

pub fn parse_search_request(model_output: &str) -> Option<String> {
    parse_search_requests(model_output).into_iter().next()
}

/// Extract ALL `SEARCH: query` lines from a model output (a complex question
/// may need several searches on different sub-topics). Deduplicated, capped at
/// 3, quotes trimmed.
pub fn parse_search_requests(model_output: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in model_output.lines() {
        let trimmed = line.trim();
        if !trimmed.to_lowercase().starts_with("search:") {
            continue;
        }
        let query = trimmed[7..].trim().trim_matches('\"').trim();
        if query.is_empty() {
            continue;
        }
        let sanitized: String = query
            .chars()
            .take(200)
            .filter(|c| !c.is_control())
            .collect();
        if seen.insert(sanitized.clone()) {
            out.push(sanitized);
            if out.len() >= 3 {
                break;
            }
        }
    }
    out
}

pub fn format_search_results(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return "\nSEARCH_RESULT: No relevant web results found.\n".to_string();
    }

    let mut formatted = String::new();
    formatted.push_str("\nSEARCH_RESULT:\n");
    for (i, res) in results.iter().enumerate() {
        formatted.push_str(&format!(
            "[{}] Title: {}\nURL: {}\nSnippet: {}\n\n",
            i + 1,
            res.title,
            res.url,
            res.snippet
        ));
    }
    formatted
}

pub fn get_search_instructions() -> &'static str {
    "\n\n=== WEB SEARCH TOOL INSTRUCTIONS ===\n\
     If you need to answer a question that requires up-to-date information (e.g., wallet features/comparisons, breaking news, latest crypto regulations, details of recent vulnerabilities), you can search the web.\n\
     CRITICAL: Do NOT use the search tool if the user's question can be answered using your existing training data (e.g. explaining static concepts, general crypto theory) or if the context already provides the necessary numbers. ONLY trigger search if you lack critical real-time information or specific recent details.\n\
     To search the web, output exactly one line starting with \"SEARCH:\" followed by your query, like this:\n\
     SEARCH: best cryptocurrency hardware wallet features 2025\n\
     Do not write anything else in that turn. Once the search results are returned, you will receive them as SEARCH_RESULT: blocks, and you can then provide your final answer.\n\
     =====================================\n"
}
