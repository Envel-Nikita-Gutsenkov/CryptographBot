pub mod budget;
pub mod dispatcher;
pub mod exa;
pub mod fetch;
pub mod local;
pub mod tavily;
pub mod tool_call;
pub mod types;

pub use budget::BudgetManager;
pub use dispatcher::search_dispatch;
pub use fetch::{fetch_page_text, jina_reader};
pub use tool_call::{
    format_search_results, get_search_instructions, parse_search_request, parse_search_requests,
};
#[allow(unused_imports)] // re-exported for integration tests
pub use types::{SearchBudget, SearchProvider, SearchQuery, SearchResult};
