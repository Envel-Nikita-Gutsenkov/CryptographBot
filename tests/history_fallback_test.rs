use cryptograph::crypto::client::CryptoClient;
use cryptograph::db::sqlite::SqliteStore;
use std::sync::Arc;

#[tokio::test]
async fn test_history_fallback_for_missing_changes() {
    let _ = dotenvy::dotenv();
    let sqlite = SqliteStore::new("sqlite::memory:").ok().map(Arc::new);
    let client = CryptoClient::new("", "", "", "", sqlite);
    
    // We request a known symbol. If it returns None for change_30d, the client logic will automatically fetch history.
    // However, our test is simply to verify that if we forcefully bypass the live price data,
    // we can calculate the 30d change manually using get_history.
    
    let symbol = "BTC";
    
    let history_res: Result<Vec<(i64, f64)>, _> = client.get_history(symbol, "30d").await;
    
    // Assert we get at least some history data back using the 4 fallback methods
    assert!(history_res.is_ok(), "Should successfully get history using one of the 4 sources (CG, Binance, CC, SQLite)");
    
    let history = history_res.unwrap();
    assert!(!history.is_empty(), "History should not be empty");
    
    // Now simulate calculating the percentage change from it
    let old_price = history.first().unwrap().1;
    assert!(old_price > 0.0, "Old price must be greater than zero");
    
    let current_price = 100000.0;
    
    let simulated_change = (current_price - old_price) / old_price * 100.0;
    
    println!("Simulated 30d change based on history: {:.2}%", simulated_change);
}
