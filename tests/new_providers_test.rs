use cryptograph::crypto::providers::{
    binance::BinanceProvider,
    bybit::BybitProvider,
    coinbase::CoinbaseProvider,
    coingecko::CoinGeckoProvider,
    rss::RssProvider,
};

#[tokio::test]
async fn test_binance_provider_real() {
    let p = BinanceProvider::new();
    let res = p.get_quotes(&["BTC", "ETH"]).await.expect("Binance failed");
    assert!(!res.is_empty(), "Binance returned empty quotes");
    for q in &res {
        println!("Binance: {} = ${}", q.symbol, q.price_usd);
        assert!(q.price_usd > 0.0);
    }
}

#[tokio::test]
async fn test_bybit_provider_real() {
    let p = BybitProvider::new();
    let res = p.get_quotes(&["BTC", "ETH"]).await.expect("Bybit failed");
    assert!(!res.is_empty(), "Bybit returned empty quotes");
    for q in &res {
        println!("Bybit: {} = ${}", q.symbol, q.price_usd);
        assert!(q.price_usd > 0.0);
    }
}

#[tokio::test]
async fn test_coinbase_provider_real() {
    let p = CoinbaseProvider::new();
    let res = p.get_quotes(&["BTC", "ETH"]).await.expect("Coinbase failed");
    assert!(!res.is_empty(), "Coinbase returned empty quotes");
    for q in &res {
        println!("Coinbase: {} = ${}", q.symbol, q.price_usd);
        assert!(q.price_usd > 0.0);
    }
}

#[tokio::test]
async fn test_rss_provider_real() {
    let p = RssProvider::new();
    
    let ct = p.get_news_cointelegraph().await.expect("CoinTelegraph RSS failed");
    assert!(!ct.is_empty(), "CoinTelegraph RSS returned 0 articles");
    println!("CoinTelegraph RSS first article: {}", ct[0].title);
    
    let cd = p.get_news_coindesk().await.expect("CoinDesk RSS failed");
    assert!(!cd.is_empty(), "CoinDesk RSS returned 0 articles");
    println!("CoinDesk RSS first article: {}", cd[0].title);
}

#[tokio::test]
async fn test_coingecko_global_real() {
    // Requires a demo api key or empty if the API supports keyless (the demo api allows query keyless sometimes or with key)
    let p = CoinGeckoProvider::new("");
    match p.get_global().await {
        Ok(g) => {
            println!("CoinGecko Global Cap: ${}", g.total_market_cap);
            assert!(g.total_market_cap > 0.0);
            assert!(g.btc_dominance > 0.0);
        }
        Err(e) => {
            println!("CoinGecko Global skipped/failed (likely rate limit or key required): {:?}", e);
        }
    }
}

#[tokio::test]
async fn test_coinpaprika_global_real() {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .unwrap();
    let resp = client.get("https://api.coinpaprika.com/v1/global")
        .header("User-Agent", "CryptographBot/1.0")
        .send()
        .await;
    match resp {
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            println!("Coinpaprika Direct status: {}, body: {}", status, text);
            assert!(status.is_success());
            let body: serde_json::Value = serde_json::from_str(&text).expect("Json parse failed");
            let cap = body["market_cap_usd"].as_f64().unwrap_or(0.0);
            assert!(cap > 0.0);
        }
        Err(e) => {
            panic!("Coinpaprika request failed: {:?}", e);
        }
    }
}

