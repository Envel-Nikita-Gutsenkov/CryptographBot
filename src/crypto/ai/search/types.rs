use std::sync::atomic::AtomicU8;
use std::sync::Arc;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub query: String,
    pub max_results: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub score: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchProvider {
    Tavily,
    Exa,
    Local,
}

#[derive(Debug, Clone)]
pub struct SearchBudget {
    pub tavily_used: Arc<AtomicU8>,
    pub exa_used: Arc<AtomicU8>,
    pub local_used: Arc<AtomicU8>,
}

impl SearchBudget {
    pub fn new() -> Self {
        Self {
            tavily_used: Arc::new(AtomicU8::new(0)),
            exa_used: Arc::new(AtomicU8::new(0)),
            local_used: Arc::new(AtomicU8::new(0)),
        }
    }
}
