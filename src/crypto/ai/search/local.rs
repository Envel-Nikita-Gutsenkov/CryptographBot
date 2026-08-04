use super::types::SearchResult;
use reqwest::Client;
use std::time::Duration;

pub async fn local_search(
    client: &Client,
    query: &str,
    max_results: usize,
) -> anyhow::Result<Vec<SearchResult>> {
    // 1) Official crypto news feeds (no keys, no captcha, no limits) ranked by
    //    keyword overlap with the query.
    let rss = crypto_rss_search(client, query, max_results).await;
    if rss.len() >= max_results {
        tracing::info!("Crypto RSS keyword search returned {} results.", rss.len());
        return Ok(rss);
    }
    // 2) Fallback: DuckDuckGo fetched server-side through Jina Reader (their
    //    infra passes the captcha that blocks direct requests).
    match jina_ddg_search(client, query, max_results).await {
        Ok(mut ddg) if !ddg.is_empty() => {
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut merged = rss;
            for r in ddg.drain(..) {
                if merged.len() >= max_results {
                    break;
                }
                if is_ru_domain(&r.url) {
                    continue; // RU domains are untrusted (ads/affiliate junk)
                }
                if seen.insert(r.url.clone()) {
                    merged.push(r);
                }
            }
            tracing::info!("Jina-DDG fallback merged, total {} results.", merged.len());
            Ok(merged)
        }
        other => {
            let e = other.err().map(|e| e.to_string()).unwrap_or_default();
            tracing::warn!("Jina-DDG search failed/empty: {e}");
            // 3) Last fallback: official Wikipedia search API (no key, no captcha).
            let wiki = wiki_search(client, query, max_results).await;
            let mut merged = rss;
            for r in wiki {
                if merged.len() >= max_results {
                    break;
                }
                merged.push(r);
            }
            if merged.is_empty() {
                Err(anyhow::anyhow!("All local search backends failed"))
            } else {
                Ok(merged)
            }
        }
    }
}

/// Official MediaWiki search API (no key, no captcha, generous limits).
/// Picks the language from the query (Cyrillic -> ru, else en).
async fn wiki_search(client: &Client, query: &str, max_results: usize) -> Vec<SearchResult> {
    // Always English — RU Wikipedia is thin and RU sources are untrusted.
    let lang = "en";
    let url = format!(
        "https://{}.wikipedia.org/w/api.php?action=query&list=search&srsearch={}&format=json&srlimit={}&srprop=snippet",
        lang,
        percent_encode(query),
        max_results
    );
    let Ok(resp) = client
        .get(&url)
        .header(
            "User-Agent",
            "cryptograph-bot/1.0 (personal crypto assistant)",
        )
        .timeout(Duration::from_secs(15))
        .send()
        .await
    else {
        return vec![];
    };
    if !resp.status().is_success() {
        return vec![];
    }
    let Ok(val) = resp.json::<serde_json::Value>().await else {
        return vec![];
    };
    let mut out = Vec::new();
    if let Some(items) = val["query"]["search"].as_array() {
        for item in items {
            let title = item["title"].as_str().unwrap_or("").to_string();
            let snippet = item["snippet"].as_str().unwrap_or("").to_string();
            let snippet = strip_html_light(&snippet);
            if title.is_empty() {
                continue;
            }
            out.push(SearchResult {
                title: title.clone(),
                url: format!(
                    "https://{}.wikipedia.org/wiki/{}",
                    lang,
                    percent_encode(&title.replace(' ', "_"))
                ),
                snippet,
                score: 0.4,
            });
        }
    }
    out
}

fn strip_html_light(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag {
            out.push(c);
        }
    }
    out
}

/// Official crypto RSS feeds (CoinTelegraph, CoinDesk, Decrypt, Blockworks,
/// TheBlock, BitcoinMagazine) — free, keyless, no captcha. Items are ranked by
/// how many query keywords appear in title+body.
pub async fn crypto_rss_search(
    _client: &Client,
    query: &str,
    max_results: usize,
) -> Vec<SearchResult> {
    let provider = crate::crypto::providers::rss::RssProvider::new();
    let futs: Vec<
        futures::future::BoxFuture<
            '_,
            crate::error::Result<Vec<crate::crypto::providers::cryptocompare::NewsArticle>>,
        >,
    > = vec![
        Box::pin(provider.get_news_cointelegraph()),
        Box::pin(provider.get_news_coindesk()),
        Box::pin(provider.get_news_decrypt()),
        Box::pin(provider.get_news_blockworks()),
        Box::pin(provider.get_news_theblock()),
        Box::pin(provider.get_news_bitcoinmagazine()),
    ];
    let all = futures::future::join_all(futs).await;
    let q_lower = query.to_lowercase();
    let q_words: std::collections::HashSet<&str> = q_lower.split_whitespace().collect();
    if q_words.is_empty() {
        return vec![];
    }
    let mut scored: Vec<(usize, SearchResult)> = Vec::new();
    for res in all {
        let Ok(articles) = res else { continue };
        for a in articles {
            if is_ru_domain(&a.url) {
                continue; // RU domains are untrusted
            }
            let hay = format!("{} {}", a.title, a.body).to_lowercase();
            let overlap = q_words.iter().filter(|w| hay.contains(*w)).count();
            if overlap == 0 {
                continue;
            }
            scored.push((
                overlap,
                SearchResult {
                    title: a.title,
                    url: a.url,
                    snippet: a.body,
                    score: overlap as f32,
                },
            ));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored
        .into_iter()
        .take(max_results)
        .map(|(_, r)| r)
        .collect()
}

/// DuckDuckGo HTML search fetched through Jina Reader
/// (`https://r.jina.ai/https://html.duckduckgo.com/html/?q=...`). Jina fetches
/// server-side and returns clean markdown, bypassing DDG's captcha.
async fn jina_ddg_search(
    client: &Client,
    query: &str,
    max_results: usize,
) -> anyhow::Result<Vec<SearchResult>> {
    let encoded = percent_encode(query);
    let url = format!(
        "https://r.jina.ai/https://html.duckduckgo.com/html/?q={}",
        encoded
    );
    let resp = client
        .get(&url)
        .timeout(Duration::from_secs(25))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("Jina-DDG HTTP {}", resp.status()));
    }
    let text = resp.text().await?;
    Ok(parse_jina_markdown(&text, max_results))
}

/// Parse Jina markdown search output: `## [Title](url)` headings followed by
/// the snippet text. Real URLs are percent-decoded from DDG's /l/?uddg= link.
fn parse_jina_markdown(text: &str, max_results: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut cur_title: Option<String> = None;
    let mut cur_url = String::new();
    let mut cur_snippet = String::new();
    let flush = |cur_title: &mut Option<String>,
                 cur_url: &mut String,
                 cur_snippet: &mut String,
                 out: &mut Vec<SearchResult>| {
        if let Some(t) = cur_title.take() {
            let url = decode_ddg_redirect(cur_url);
            if !url.is_empty() && !t.is_empty() {
                out.push(SearchResult {
                    title: t,
                    url,
                    snippet: crate::crypto::ai::search::tavily::clean_markdown_links(
                        &std::mem::take(cur_snippet),
                    ),
                    score: 0.5,
                });
            }
            cur_snippet.clear();
        }
    };
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("## [") {
            flush(&mut cur_title, &mut cur_url, &mut cur_snippet, &mut results);
            if results.len() >= max_results {
                break;
            }
            if let Some(rest) = trimmed.strip_prefix("## [") {
                if let Some(end) = rest.find("](") {
                    cur_title = Some(rest[..end].to_string());
                    cur_url = rest[end + 2..].trim_end_matches(')').trim().to_string();
                }
            }
        } else if cur_title.is_some() && !trimmed.is_empty() {
            if cur_snippet.len() < 600 {
                cur_snippet.push_str(trimmed);
                cur_snippet.push(' ');
            }
        }
    }
    flush(&mut cur_title, &mut cur_url, &mut cur_snippet, &mut results);
    results.truncate(max_results);
    results
}

/// Extract the real URL from a DuckDuckGo /l/?uddg= redirect and percent-decode
/// it. If the link is not a DDG redirect, use it as-is.
fn decode_ddg_redirect(raw: &str) -> String {
    let decoded = percent_decode(raw);
    let Some(pos) = decoded.find("uddg=") else {
        return decoded;
    };
    let mut target = decoded[pos + 5..].to_string();
    if let Some(amp) = target.find('&') {
        target.truncate(amp);
    }
    percent_decode(&target)
}

fn percent_decode(s: &str) -> String {
    let bytes: Vec<u8> = s.as_bytes().to_vec();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Russian domains (host .ru or ru.* subdomain) are treated as untrusted and
/// filtered out of search results.
fn is_ru_domain(url: &str) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .to_lowercase()
        .trim_end_matches('.')
        .to_string();
    host.ends_with(".ru") || host == "ru" || host.starts_with("ru.")
}
