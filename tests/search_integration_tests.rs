use dotenvy::dotenv;
use reqwest::Client;
use cryptograph::crypto::ai::search::{
    BudgetManager, SearchBudget, SearchProvider, search_dispatch,
    local::local_search,
};

#[tokio::test]
async fn test_search_budget() {
    let budget = SearchBudget::new();
    let mgr = BudgetManager::new(budget);

    // Test Tavily limit (3)
    assert!(mgr.try_consume(SearchProvider::Tavily, 3));
    assert!(mgr.try_consume(SearchProvider::Tavily, 3));
    assert!(mgr.try_consume(SearchProvider::Tavily, 3));
    assert!(!mgr.try_consume(SearchProvider::Tavily, 3)); // 4th call should fail

    // Test Exa limit (3)
    assert!(mgr.try_consume(SearchProvider::Exa, 3));
    assert!(mgr.try_consume(SearchProvider::Exa, 3));
    assert!(mgr.try_consume(SearchProvider::Exa, 3));
    assert!(!mgr.try_consume(SearchProvider::Exa, 3));

    // Test Local limit (10)
    for _ in 0..10 {
        assert!(mgr.try_consume(SearchProvider::Local, 10));
    }
    assert!(!mgr.try_consume(SearchProvider::Local, 10));
}

#[tokio::test]
async fn test_local_search_engines() {
    let client = Client::new();
    let query = "bitcoin price prediction 2026";
    println!("Testing local search with query: {}", query);
    
    match local_search(&client, query, 3).await {
        Ok(results) => {
            println!("Local search success! Found {} results.", results.len());
            assert!(!results.is_empty(), "Should return at least some results");
            for (i, res) in results.iter().enumerate() {
                println!("{}. Title: {}, URL: {}", i + 1, res.title, res.url);
                assert!(!res.title.is_empty(), "Result title should not be empty");
                assert!(!res.url.is_empty(), "Result url should not be empty");
            }
        }
        Err(e) => {
            panic!("Local search failed completely: {:?}", e);
        }
    }
}

#[tokio::test]
async fn test_dispatcher_tavily_exa() {
    dotenv().ok();
    let client = Client::new();
    let budget = SearchBudget::new();
    let mgr = BudgetManager::new(budget);

    let query = "ethereum latest news";
    
    // Dispatch search
    let results = search_dispatch(&client, &mgr, query, false).await;
    println!("Dispatcher returned {} results.", results.len());
    
    // Even if APIs fail, local fallback should ensure we have results (unless network issue)
    for res in &results {
        assert!(!res.url.is_empty());
        assert!(!res.title.is_empty());
    }
}
