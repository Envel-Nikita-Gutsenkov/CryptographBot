//! On-chain balance polling for BTC, ETH, SOL wallets.
//! Uses public APIs — no extra API key needed for BTC/ETH basics.

use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::{
    error::{BotError, Result},
    models::Network,
};

pub struct BlockchainClient {
    http: Client,
    helius_api_key: String,
    ankr_rpc_url: String,
}

impl BlockchainClient {
    pub fn new(helius_api_key: impl Into<String>, ankr_rpc_url: impl Into<String>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .use_rustls_tls()
            .pool_max_idle_per_host(4)
            .build()
            .expect("HTTP client");

        Self {
            http,
            helius_api_key: helius_api_key.into(),
            ankr_rpc_url: ankr_rpc_url.into(),
        }
    }

    /// Fetch balance for the given address on the given network.
    /// Returns balance in the native unit (BTC, ETH, SOL, …).
    pub async fn get_balance(&self, address: &str, network: &Network) -> Result<f64> {
        match network {
            Network::Bitcoin => self.btc_balance(address).await,
            Network::Ethereum => self.eth_balance(address, "ETH").await,
            Network::Solana => self.sol_balance(address).await,
            Network::Tron => self.trx_balance(address).await,
            Network::Bsc => self.eth_balance(address, "BNB").await, // EVM-compatible
            Network::Generic(n) => {
                warn!("No balance checker for network {n}");
                Err(BotError::BlockchainApi(format!(
                    "Unsupported network: {n}"
                )))
            }
        }
    }

    async fn btc_balance(&self, address: &str) -> Result<f64> {
        debug!("BTC balance: {address}");
        #[derive(Deserialize)]
        struct Resp {
            chain_stats: Stats,
        }
        #[derive(Deserialize)]
        struct Stats {
            funded_txo_sum: u64,
            spent_txo_sum: u64,
        }

        let url = format!("https://blockstream.info/api/address/{address}");
        let resp: Resp = self.http.get(&url).send().await?.json().await?;

        let satoshis = resp
            .chain_stats
            .funded_txo_sum
            .saturating_sub(resp.chain_stats.spent_txo_sum);
        Ok(satoshis as f64 / 1e8)
    }

    async fn eth_balance(&self, address: &str, native_symbol: &str) -> Result<f64> {
        debug!("EVM Multichain balance for {}: {}", native_symbol, address);
        
        if self.ankr_rpc_url.is_empty() {
            warn!("Ankr RPC URL not configured, EVM balances won't work.");
            return Ok(0.0);
        }

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "ankr_getAccountBalance",
            "params": {
                "blockchain": ["eth", "bsc", "polygon", "base", "arbitrum", "avalanche"],
                "walletAddress": address
            },
            "id": 1
        });

        let resp: serde_json::Value = self
            .http
            .post(&self.ankr_rpc_url)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;

        if resp.get("error").is_some() {
            return Err(BotError::BlockchainApi(format!("Ankr error: {:?}", resp["error"])));
        }

        // We want to return the "equivalent" amount of the native token (ETH or BNB)
        // so that when the bot multiplies it by the native token's price, it perfectly matches the total USD value.
        let mut total_usd = 0.0;
        let mut native_price = 0.0;
        
        if let Some(assets) = resp["result"]["assets"].as_array() {
            for asset in assets {
                let bal_usd = asset["balanceUsd"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
                total_usd += bal_usd;
                
                let sym = asset["tokenSymbol"].as_str().unwrap_or("");
                if sym == native_symbol {
                    let price = asset["tokenPrice"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
                    if price > 0.0 {
                        native_price = price;
                    }
                }
            }
        }

        if native_price > 0.0 {
            // Faked amount of native tokens representing the total USD value of the ENTIRE multichain portfolio
            Ok(total_usd / native_price)
        } else {
            // Fallback if they don't hold any of the native token (so we can't get its price)
            // Just return 0 to avoid breaking math
            Ok(0.0)
        }
    }

    async fn sol_balance(&self, address: &str) -> Result<f64> {
        debug!("SOL balance: {address}");
        let url = if self.helius_api_key.is_empty() {
            "https://api.mainnet-beta.solana.com".to_string()
        } else {
            format!(
                "https://mainnet.helius-rpc.com/?api-key={}",
                self.helius_api_key
            )
        };

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBalance",
            "params": [address]
        });

        let resp: serde_json::Value =
            self.http.post(&url).json(&body).send().await?.json().await?;

        let lamports = resp["result"]["value"]
            .as_u64()
            .ok_or_else(|| BotError::BlockchainApi("No value".into()))?;

        Ok(lamports as f64 / 1e9)
    }

    async fn trx_balance(&self, address: &str) -> Result<f64> {
        debug!("TRX balance: {address}");

        // TronGrid public API
        let url = format!("https://api.trongrid.io/v1/accounts/{address}/bandwidth");
        let resp: serde_json::Value = self.http.get(&url).send().await?.json().await?;

        let sun = resp["data"][0]["balance"].as_u64().unwrap_or(0);
        Ok(sun as f64 / 1_000_000.0)
    }
}
