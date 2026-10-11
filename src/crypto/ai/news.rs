use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use std::sync::OnceLock;
use tracing::info;

struct NewsCache {
    rss_body: String,
    fetched_at: DateTime<Utc>,
}

static NEWS_CACHE: OnceLock<Mutex<Option<NewsCache>>> = OnceLock::new();

pub async fn fetch_news_for_symbol(
    symbol: &str,
    coin_name: &str,
    limit: usize,
) -> anyhow::Result<String> {
    let now = Utc::now();
    let cache_cell = NEWS_CACHE.get_or_init(|| Mutex::new(None));

    // Copy cache value out of the lock scope
    let cached_body = {
        let guard = cache_cell.lock();
        if let Some(cache) = &*guard {
            Some((cache.rss_body.clone(), cache.fetched_at))
        } else {
            None
        }
    };

    let mut body = String::new();

    // Check if cache is fresh
    if let Some((cached_text, fetched_at)) = &cached_body {
        if now.signed_duration_since(*fetched_at).num_minutes() < 30 {
            body = cached_text.clone();
        }
    }

    if body.is_empty() {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .use_rustls_tls()
            .build()?;

        match client.get("https://cointelegraph.com/rss").send().await {
            Ok(res) => {
                if let Ok(text) = res.text().await {
                    body = text.clone();
                    let mut guard = cache_cell.lock();
                    *guard = Some(NewsCache {
                        rss_body: text,
                        fetched_at: now,
                    });
                }
            }
            Err(e) => {
                info!("RSS fetch failed: {}", e);
            }
        }

        // Fallback: serve stale cache if network fails
        if body.is_empty() {
            if let Some((cached_text, _)) = cached_body {
                body = cached_text;
            } else {
                return Ok("No recent news found.".to_string());
            }
        }
    }

    let mut articles = Vec::new();
    let mut rest = body.as_str();
    let sym_lower = symbol.to_lowercase();
    let name_lower = coin_name.to_lowercase();

    while let Some(start_idx) = rest.find("<item>") {
        let item_block = &rest[start_idx..];
        let end_idx = match item_block.find("</item>") {
            Some(idx) => idx,
            None => break,
        };
        let item = &item_block[..end_idx];
        rest = &item_block[end_idx + 7..];

        let title = extract_xml_tag(item, "title").unwrap_or_default();
        let description = extract_xml_tag(item, "description").unwrap_or_default();
        let link = extract_xml_tag(item, "link").unwrap_or_default();

        let title_lower = title.to_lowercase();
        let desc_lower = description.to_lowercase();

        let matches = title_lower.contains(&sym_lower)
            || desc_lower.contains(&sym_lower)
            || title_lower.contains(&name_lower)
            || desc_lower.contains(&name_lower);

        if matches {
            articles.push((title, description, link));
            if articles.len() >= limit {
                break;
            }
        }
    }

    if articles.is_empty() {
        // Fallback: if no specific coin news, return top general articles up to limit
        let mut rest = body.as_str();
        while let Some(start_idx) = rest.find("<item>") {
            let item_block = &rest[start_idx..];
            let end_idx = match item_block.find("</item>") {
                Some(idx) => idx,
                None => break,
            };
            let item = &item_block[..end_idx];
            rest = &item_block[end_idx + 7..];

            let title = extract_xml_tag(item, "title").unwrap_or_default();
            let description = extract_xml_tag(item, "description").unwrap_or_default();
            let link = extract_xml_tag(item, "link").unwrap_or_default();

            articles.push((title, description, link));
            if articles.len() >= limit {
                break;
            }
        }
    }

    let mut summary = String::new();
    for (i, (title, desc, _link)) in articles.iter().enumerate() {
        let clean_title = strip_html_tags(title);
        let clean_desc = strip_html_tags(desc);
        summary.push_str(&format!(
            "{}. {}\n   Summary: {}\n\n",
            i + 1,
            clean_title,
            clean_desc
        ));
    }
    Ok(summary)
}

fn strip_html_tags(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut in_tag = false;
    for c in input.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ => {
                if !in_tag {
                    result.push(c);
                }
            }
        }
    }
    result
        .replace("\n", " ")
        .replace("\r", "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open_tag = format!("<{}>", tag);
    let close_tag = format!("</{}>", tag);
    let start_idx = xml.find(&open_tag)?;
    let start = start_idx + open_tag.len();
    let end = xml.find(&close_tag)?;
    if start >= end {
        return None;
    }

    let content = xml[start..end].trim();
    let content = if content.starts_with("<![CDATA[") && content.ends_with("]]>") {
        &content[9..content.len() - 3]
    } else {
        content
    };
    Some(content.to_string())
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    #[cfg(test)]
    #[derive(Serialize, Deserialize, Debug)]
    #[serde(rename_all = "UPPERCASE")]
    struct NewsArticle {
        title: String,
        body: String,
        url: String,
        categories: String,
    }

    #[test]
    fn test_news_filtering() {
        let articles = vec![
            NewsArticle {
                title: "Bitcoin breaks above 100k".to_string(),
                body: "BTC price is skyrocketing today.".to_string(),
                url: "http://example.com/1".to_string(),
                categories: "BTC|Market".to_string(),
            },
            NewsArticle {
                title: "Ethereum reaches 4k".to_string(),
                body: "ETH is showing strong momentum.".to_string(),
                url: "http://example.com/2".to_string(),
                categories: "ETH|SmartContracts".to_string(),
            },
            NewsArticle {
                title: "Cardano upgrades node".to_string(),
                body: "ADA is preparing for hard fork.".to_string(),
                url: "http://example.com/3".to_string(),
                categories: "ADA".to_string(),
            },
        ];

        // Filter for BTC
        let mut filtered_btc = Vec::new();
        let sym_upper = "BTC";
        let name_lower = "bitcoin";
        for art in &articles {
            let title_lower = art.title.to_lowercase();
            let body_lower = art.body.to_lowercase();
            let cat_upper = art.categories.to_uppercase();

            let matches_symbol = cat_upper.split('|').any(|c| c == sym_upper)
                || title_lower.contains(&sym_upper.to_lowercase())
                || body_lower.contains(&sym_upper.to_lowercase());

            let matches_name = title_lower.contains(name_lower) || body_lower.contains(name_lower);

            if matches_symbol || matches_name {
                filtered_btc.push(art);
            }
        }

        assert_eq!(filtered_btc.len(), 1);
        assert_eq!(filtered_btc[0].title, "Bitcoin breaks above 100k");
    }
}
