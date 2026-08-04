pub mod client;
pub mod deepseek_budget;
pub mod macro_worker;
pub mod market_intel_worker;
pub mod metrics;
pub mod news;
pub mod news_worker;
pub mod pipeline;
pub mod pools;
pub mod proactive_worker;
pub mod prompts;
pub mod providers;
pub mod search;
pub mod watcher;

pub use client::GeminiClient;
pub use news::fetch_news_for_symbol;
