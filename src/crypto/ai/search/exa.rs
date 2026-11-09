use super::types::SearchResult;
use reqwest::Client;
use std::time::Duration;

pub async fn exa_search(
    client: &Client,
    api_key: &str,
    query: &str,
    max_results: usize,
) -> anyhow::Result<Vec<SearchResult>> {
    let url = "https://api.exa.ai/search";

    let body = serde_json::json!({
        "query": query,
        "type": "auto",
        "numResults": max_results,
        "contents": {
            "highlights": true
        }
    });

    let resp = client
        .post(url)
        .header("x-api-key", api_key)
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(12))
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let err_body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Exa HTTP {status}: {err_body}"));
    }

    let val = resp.json::<serde_json::Value>().await?;
    let mut results = Vec::new();

    if let Some(arr) = val["results"].as_array() {
        for item in arr {
            let title = item["title"].as_str().unwrap_or("").to_string();
            let url = item["url"].as_str().unwrap_or("").to_string();

            // Join highlights as snippet
            let mut snippet = String::new();
            if let Some(highlights_arr) = item["highlights"].as_array() {
                for hl in highlights_arr {
                    if let Some(hl_str) = hl.as_str() {
                        if !snippet.is_empty() {
                            snippet.push_str(" ... ");
                        }
                        snippet.push_str(hl_str);
                    }
                }
            }

            // Fallback to text if highlights are empty (safety check)
            if snippet.is_empty() {
                snippet = item["text"].as_str().unwrap_or("").trim().to_string();
            }

            if snippet.len() > 500 {
                let end = snippet
                    .char_indices()
                    .map(|(i, _)| i)
                    .nth(500)
                    .unwrap_or(snippet.len());
                snippet = snippet[..end].to_string();
                snippet.push_str("...");
            }

            let score = item["score"].as_f64().unwrap_or(0.0) as f32;

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
