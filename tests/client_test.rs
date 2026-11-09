use cryptograph::crypto::client::CryptoClient;
use cryptograph::format::fmt_quote_card;
use cryptograph::models::CmcQuote;

#[tokio::test]
async fn test_live_api_fetch() {
    // Tests that we can fetch BTC and it has expected fields populated.
    let client = CryptoClient::new("dummy", "dummy", "dummy", "dummy", None);
    let quotes = client
        .get_quotes(&["BTC"])
        .await
        .expect("Failed to get quotes");

    assert!(!quotes.is_empty(), "Expected at least 1 quote");
    let btc = &quotes[0];

    // Even without valid API keys for CMC/CG, the fallback to Binance/Bybit should give us a valid price.
    assert!(btc.price_usd > 0.0, "BTC price should be > 0");
    assert_eq!(btc.symbol, "BTC");

    // Print the final formatted card so we can see it with real data
    println!("=== REAL FORMATTED CARD ===");
    println!("{}", fmt_quote_card(btc));
    println!("===========================");
}

#[tokio::test]
async fn test_news_fallback() {
    let client = CryptoClient::new("dummy", "dummy", "dummy", "dummy", None);
    let news = client.get_news("BTC").await.expect("Failed to fetch news");
    // With RSS fallback, it should return some news even if CryptoCompare needs an API key
    assert!(
        !news.is_empty(),
        "News list should not be empty, RSS fallback should work"
    );
}

#[test]
fn test_fmt_quote_card_missing_data() {
    // Test that when rank is 0, we output N/A instead of Rank #0 or $0.00
    let missing_quote = CmcQuote {
        symbol: "TEST".to_string(),
        name: "Test Coin".to_string(),
        price_usd: 1.23,
        market_cap: 0.0,
        volume_24h: 0.0,
        change_1h: None,
        change_24h: None,
        change_7d: None,
        change_30d: None,
        change_90d: None,
        change_365d: None,
        change_730d: None,
        ath: None,
        ath_change_percentage: None,
        high_24h: None,
        low_24h: None,
        rank: 0,
        circulating_supply: 0.0,
        last_updated: chrono::Utc::now(),
        anomaly_warning: None,
    };

    let formatted = fmt_quote_card(&missing_quote);
    assert!(
        formatted.contains("`Rank: N/A`"),
        "Should contain Rank: N/A. Got: {}",
        formatted
    );
    assert!(
        formatted.contains("*N/A*"),
        "Should contain N/A for market cap and volume. Got: {}",
        formatted
    );
}

#[test]
fn test_fmt_quote_card_with_data() {
    let good_quote = CmcQuote {
        symbol: "BTC".to_string(),
        name: "Bitcoin".to_string(),
        price_usd: 60000.0,
        market_cap: 1_200_000_000.0,
        volume_24h: 30_000_000_000.0,
        change_1h: Some(1.0),
        change_24h: Some(5.0),
        change_7d: Some(-2.0),
        change_30d: Some(10.0),
        change_90d: Some(20.0),
        change_365d: Some(50.0),
        change_730d: Some(100.0),
        ath: Some(69000.0),
        ath_change_percentage: Some(-13.0),
        high_24h: Some(61000.0),
        low_24h: Some(59000.0),
        rank: 1,
        circulating_supply: 19_000_000.0,
        last_updated: chrono::Utc::now(),
        anomaly_warning: None,
    };

    let formatted = fmt_quote_card(&good_quote);
    assert!(formatted.contains("`Rank \\#1`"), "Should contain Rank #1");
    assert!(formatted.contains("1\\.20B"), "Should format market cap");
}
