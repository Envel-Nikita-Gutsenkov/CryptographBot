use std::time::Duration;

use reqwest::Client;

/// Jina Reader — free server-side page-to-text: `https://r.jina.ai/<url>`
/// returns clean markdown. No API key needed; their infra does the fetching,
/// so Cloudflare-protected sites work. Best-effort, rate-limited by them.
pub async fn jina_reader(client: &Client, url: &str, max_chars: usize) -> Option<String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let reader_url = format!("https://r.jina.ai/{}", url);
    let resp = client
        .get(&reader_url)
        // Jina returns 403 for browser-like UAs from non-browser TLS clients;
        // a plain custom UA is accepted.
        .header("User-Agent", "cryptograph/1.0")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let text = resp.text().await.ok()?;
    // Jina prepends metadata (Title/URL/Markdown). Keep everything; the
    // summarizer/relevance filter handles noise.
    if text.chars().count() < 200 {
        return None;
    }
    Some(
        crate::crypto::ai::search::fetch::strip_urls(&text)
            .chars()
            .take(max_chars)
            .collect(),
    )
}

/// Remove bare URLs (http/https/www) from extracted text — share-link junk,
/// percent-encoded bodies and image URLs are pure noise for the models.
pub fn strip_urls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(|c: char| c == 'h' || c == 'w') {
        let tail = &rest[pos..];
        let is_url = tail.starts_with("http://")
            || tail.starts_with("https://")
            || tail.starts_with("www.")
            || tail.starts_with("WWW.");
        if !is_url {
            out.push(rest.as_bytes()[pos] as char);
            rest = &tail[1..];
            continue;
        }
        out.push_str(&rest[..pos]);
        let end = tail
            .find(|c: char| {
                c.is_whitespace()
                    || c == ')'
                    || c == ']'
                    || c == '"'
                    || c == '\''
                    || c == '>'
                    || c == '<'
            })
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Fetch a web page and return its readable text (scripts/styles stripped,
/// tags removed, whitespace collapsed), capped at `max_chars`. Returns None on
/// any failure (non-HTML content, non-2xx, timeout, too little text) so the
/// caller can fall back to search snippets.
pub async fn fetch_page_text(client: &Client, url: &str, max_chars: usize) -> Option<String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let resp = client
        .get(url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36",
        )
        .header("Accept", "text/html,application/xhtml+xml")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_lowercase();
    if !content_type.is_empty() && !content_type.contains("html") && !content_type.contains("text/")
    {
        return None; // pdf, images, json — skip
    }
    let html = resp.text().await.ok()?;
    let text = strip_urls(&html_to_text(&html));
    if text.chars().count() < 200 {
        return None; // too little content (login wall, JS-only page, 404 page)
    }
    Some(text.chars().take(max_chars).collect())
}

fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut skip = false; // inside <script>/<style> — drop until closing tag
    let mut skip_close = String::new();
    let mut i = 0;
    let bytes: Vec<char> = html.chars().collect();
    while i < bytes.len() {
        let c = bytes[i];
        if c == '<' {
            // Read the tag name to detect script/style blocks.
            let mut j = i + 1;
            let mut name = String::new();
            while j < bytes.len() {
                let nc = bytes[j];
                if nc.is_ascii_alphanumeric() || nc == '/' {
                    name.push(nc.to_ascii_lowercase());
                    j += 1;
                } else {
                    break;
                }
            }
            if name.contains("script") || name.contains("style") {
                skip = true;
                skip_close = format!("/{}", name);
                i = j;
                continue;
            }
            if skip && name == skip_close {
                skip = false;
                skip_close.clear();
                i = j;
                continue;
            }
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag && !skip {
            out.push(c);
        }
        i += 1;
    }
    // Collapse whitespace and keep paragraphs readable-ish.
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
}
