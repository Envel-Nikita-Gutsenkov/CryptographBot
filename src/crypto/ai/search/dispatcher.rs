use super::budget::BudgetManager;
use super::exa::exa_search;
use super::local::local_search;
use super::tavily::tavily_search;
use super::types::{SearchProvider, SearchResult};
use reqwest::Client;
use std::collections::HashSet;

pub async fn search_dispatch(
    client: &Client,
    budget: &BudgetManager,
    query: &str,
    hint_local_only: bool,
) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let clean_query = query.trim();
    if clean_query.is_empty() {
        return results;
    }

    let tavily_key = std::env::var("TAVILY_API_KEY").unwrap_or_default();
    let exa_key = std::env::var("EXA_API_KEY").unwrap_or_default();

    let mut provider_order = Vec::new();
    if hint_local_only {
        provider_order.push(SearchProvider::Local);
    } else {
        if !tavily_key.is_empty() {
            provider_order.push(SearchProvider::Tavily);
        }
        if !exa_key.is_empty() {
            provider_order.push(SearchProvider::Exa);
        }
        provider_order.push(SearchProvider::Local);
    }

    for provider in provider_order {
        let max_calls = match provider {
            SearchProvider::Tavily => 3,
            SearchProvider::Exa => 3,
            SearchProvider::Local => 10,
        };

        if !budget.try_consume(provider, max_calls) {
            tracing::warn!(
                "Search provider {:?} budget limit reached. Skipping.",
                provider
            );
            continue;
        }

        tracing::info!(
            "Executing search query '{}' using {:?}",
            clean_query,
            provider
        );

        let res = match provider {
            SearchProvider::Tavily => tavily_search(client, &tavily_key, clean_query, 5).await,
            SearchProvider::Exa => exa_search(client, &exa_key, clean_query, 5).await,
            SearchProvider::Local => {
                let query_lower = clean_query.to_lowercase();
                let wants_more = query_lower.contains("detail")
                    || query_lower.contains("compare")
                    || query_lower.contains("comparison")
                    || query_lower.contains("full")
                    || query_lower.contains("list")
                    || query_lower.contains("all")
                    || query_lower.contains("анализ")
                    || query_lower.contains("сравн")
                    || query_lower.contains("подробн")
                    || query_lower.contains("все");
                let count = if wants_more { 10 } else { 5 };
                local_search(client, clean_query, count).await
            }
        };

        // Per-provider usage stats: a provider counts as OK when it actually
        // returned results (it did the job), ERR when it failed or found nothing.
        let provider_name = match provider {
            SearchProvider::Tavily => "Tavily",
            SearchProvider::Exa => "Exa",
            SearchProvider::Local => "Local",
        };
        match &res {
            Ok(found) if !found.is_empty() => {
                super::super::metrics::record_search_success(provider_name);
            }
            _ => {
                super::super::metrics::record_search_error(provider_name);
            }
        }

        // Debug: show every web search the model triggers + what came back.
        if std::env::var("CRYPTOGRAPH_DEBUG_PROMPTS").unwrap_or_default() == "1" {
            match &res {
                Ok(found) => {
                    println!(
                        "\n🔎 WEB SEARCH [{provider_name}] query=\"{clean_query}\" -> {} results",
                        found.len()
                    );
                    for r in found.iter().take(5) {
                        let snippet: String = r.snippet.chars().take(140).collect();
                        println!("   • {} | {}\n     {}", r.title, r.url, snippet);
                    }
                }
                Err(e) => println!(
                    "\n🔎 WEB SEARCH [{provider_name}] query=\"{clean_query}\" FAILED: {}",
                    e
                ),
            }
        }

        match res {
            Ok(found) => {
                if !found.is_empty() {
                    results = found;
                    break;
                }
            }
            Err(e) => {
                tracing::warn!("Search provider {:?} failed: {:?}", provider, e);
                // Keep trying next provider
            }
        }
    }

    // Deduplicate results by URL
    let mut unique_urls = HashSet::new();
    let mut deduped = Vec::new();
    for item in results {
        if unique_urls.insert(item.url.clone()) {
            deduped.push(item);
        }
    }

    deduped
}
