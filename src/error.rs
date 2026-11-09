use thiserror::Error;

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum BotError {
    #[error("Unauthorized: admin access required")]
    Unauthorized,

    #[error("CoinMarketCap API error: {0}")]
    CmcApi(String),

    #[error("Rate limit exceeded, please try again later")]
    RateLimited,

    #[error("Symbol not found: {0}")]
    SymbolNotFound(String),

    #[error("Encryption error: {0}")]
    Encryption(String),

    #[error("Decryption failed — invalid key or corrupted data")]
    DecryptionFailed,

    #[error("Invalid backup file")]
    InvalidBackup,

    #[error("Wallet address not found or invalid")]
    WalletNotFound,

    #[error("Chart generation failed: {0}")]
    ChartError(String),

    #[error("Blockchain API error: {0}")]
    BlockchainApi(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("Internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, BotError>;
