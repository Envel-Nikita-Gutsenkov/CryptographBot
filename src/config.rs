use anyhow::{Context, Result};
use rand::RngCore;
use std::str::FromStr;
use tracing::info;
use zeroize::Zeroizing;

#[derive(Clone)]
#[allow(dead_code)]
pub struct Config {
    pub telegram_token: String,
    pub allowed_admin_ids: Vec<u64>,
    pub cmc_api_key: String,
    pub coingecko_api_key: String,
    pub coincap_api_key: String,
    pub cryptocompare_api_key: String,
    pub helius_api_key: String,
    pub ankr_rpc_url: String,
    pub gemini_api_key: String,
    pub session_key: Zeroizing<[u8; 32]>,
    pub alert_interval_secs: u64,
    pub wallet_poll_mins: u64,
    /// Path to SQLite database file. Configurable via SQLITE_DB env var.
    /// Defaults to "crypto.db" for local dev, override to "/app/data/crypto.db" in Docker.
    pub sqlite_db: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();
        let telegram_token = req("TELOXIDE_TOKEN")?;
        let cmc_api_key = req("CMC_API_KEY")?;
        let coingecko_api_key = std::env::var("COINGECKO_API_KEY").unwrap_or_default();
        let coincap_api_key = std::env::var("COINCAP_API_KEY").unwrap_or_default();
        let cryptocompare_api_key = std::env::var("CRYPTOCOMPARE_API_KEY").unwrap_or_default();
        let helius_api_key = std::env::var("HELIUS_API_KEY").unwrap_or_default();
        let ankr_rpc_url = std::env::var("ANKR_RPC_URL").unwrap_or_default();
        let gemini_api_key = std::env::var("GEMINI_API_KEY").unwrap_or_default();

        let allowed_admin_ids = req("ALLOWED_ADMIN_IDS")?
            .split(',')
            .map(|s| s.trim().parse::<u64>())
            .collect::<Result<Vec<_>, _>>()
            .context("ALLOWED_ADMIN_IDS must be comma-separated u64")?;

        let mut session_key = Zeroizing::new([0u8; 32]);
        rand::thread_rng().fill_bytes(session_key.as_mut());
        info!("Ephemeral session key generated");

        Ok(Self {
            telegram_token,
            allowed_admin_ids,
            cmc_api_key,
            coingecko_api_key,
            coincap_api_key,
            cryptocompare_api_key,
            helius_api_key,
            ankr_rpc_url,
            gemini_api_key,
            session_key,
            alert_interval_secs: env_u64("ALERT_INTERVAL_SECS", 300)?,
            wallet_poll_mins: env_u64("WALLET_POLL_MINS", 15)?,
            sqlite_db: std::env::var("SQLITE_DB").unwrap_or_else(|_| "crypto.db".to_string()),
        })
    }

    pub fn is_admin(&self, user_id: u64) -> bool {
        self.allowed_admin_ids.contains(&user_id)
    }
}

fn req(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("Missing env: {key}"))
}

fn env_u64(key: &str, default: u64) -> Result<u64> {
    match std::env::var(key) {
        Ok(v) => u64::from_str(&v).with_context(|| format!("{key} must be u64")),
        Err(_) => Ok(default),
    }
}
