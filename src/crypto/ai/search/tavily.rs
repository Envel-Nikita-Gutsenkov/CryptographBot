use super::types::SearchResult;
use reqwest::Client;
use std::time::Duration;

pub async fn tavily_search(
    client: &Client,
    api_key: &str,
    query: &str,
    max_results: usize,
) -> anyhow::Result<Vec<SearchResult>> {
    let url = "https://api.tavily.com/search";

    let body = serde_json::json!({
        "api_key": api_key,
        "query": query,
        "search_depth": "advanced",
        "max_results": max_results
    });

    let resp = client
        .post(url)
        .timeout(Duration::from_secs(10))
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let err_body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Tavily HTTP {status}: {err_body}"));
    }

    let val = resp.json::<serde_json::Value>().await?;
    let mut results = Vec::new();

    if let Some(arr) = val["results"].as_array() {
        for item in arr {
            let title = item["title"].as_str().unwrap_or("").to_string();
            let url = item["url"].as_str().unwrap_or("").to_string();
            let content = item["content"].as_str().unwrap_or("").to_string();
            let score = item["score"].as_f64().unwrap_or(0.0) as f32;

            // Clean/truncate content to 500 chars to avoid model context bloat
            let mut snippet = content.trim().to_string();
            if snippet.len() > 500 {
                let end = snippet
                    .char_indices()
                    .map(|(i, _)| i)
                    .nth(500)
                    .unwrap_or(snippet.len());
                snippet = snippet[..end].to_string();
                snippet.push_str("...");
            }

            results.push(SearchResult {
                title,
                url,
                snippet,
                score,
            });
        }
    }

    Ok(results)
}

/// Tavily Extract — fetches full page content server-side (their infra, not
/// ours), which bypasses Cloudflare/anti-bot blocks on our own rustls client.
/// Returns (url, cleaned text) pairs for urls that produced usable content.
pub async fn tavily_extract(
    client: &Client,
    api_key: &str,
    urls: &[String],
    max_chars: usize,
) -> Vec<(String, String)> {
    if urls.is_empty() {
        return vec![];
    }
    let body = serde_json::json!({
        "api_key": api_key,
        "urls": urls,
        "extract_depth": "advanced",
    });
    let resp = client
        .post("https://api.tavily.com/extract")
        .timeout(Duration::from_secs(30))
        .json(&body)
        .send()
        .await;
    let Ok(resp) = resp else {
        return vec![];
    };
    if !resp.status().is_success() {
        return vec![];
    }
    let Ok(val) = resp.json::<serde_json::Value>().await else {
        return vec![];
    };
    let mut out = Vec::new();
    if let Some(results) = val["results"].as_array() {
        for item in results {
            let url = item["url"].as_str().unwrap_or("").to_string();
            let content = item["content"]
                .as_str()
                .or_else(|| item["raw_content"].as_str())
                .unwrap_or("")
                .to_string();
            let text =
                crate::crypto::ai::search::fetch::strip_urls(&clean_markdown_links(&content))
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
            if text.chars().count() >= 200 {
                out.push((url, text.chars().take(max_chars).collect()));
            }
        }
    }
    out
}

/// Remove markdown noise: `![alt](url)` images entirely, `[text](url)` links
/// down to their text. Two passes handle the nested DDG pattern
/// `[![Image 1](img)](redirect)`, which becomes an empty link and is dropped.
pub fn clean_markdown_links(input: &str) -> String {
    let mut s = input.to_string();
    // Pass A: drop images ![alt](url)
    loop {
        let Some(start) = s.find("![") else { break };
        let Some(brace_rel) = s[start..].find("](") else {
            break;
        };
        let brace = start + brace_rel;
        let Some(close_rel) = s[brace..].find(')') else {
            break;
        };
        let close = brace + close_rel;
        s.replace_range(start..=close, "");
    }
    // Pass B: [text](url) -> text (empty links drop entirely)
    loop {
        let Some(start) = s.find('[') else { break };
        let Some(brace_rel) = s[start..].find("](") else {
            break;
        };
        let brace = start + brace_rel;
        let Some(close_rel) = s[brace..].find(')') else {
            break;
        };
        let close = brace + close_rel;
        let text = s[start + 1..brace].to_string();
        s.replace_range(start..=close, &text);
    }
    s
}
