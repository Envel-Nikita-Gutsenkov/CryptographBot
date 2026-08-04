use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UserSettings {
    pub default_watchlist: Vec<String>,
    pub currency_display: CurrencyDisplay,
    pub backup_on_change: bool,
    pub alert_quiet_hours_start: Option<u8>,
    pub alert_quiet_hours_end: Option<u8>,
    /// Preferred AI model ID (e.g. "gemini-2.5-flash", "mistral-large-2512").
    pub ai_model_override: Option<String>,
    /// Watcher cooldown tracking.
    pub ai_last_critical_notify: Option<DateTime<Utc>>,
    pub ai_last_important_notify: Option<DateTime<Utc>>,
    pub ai_watcher_enabled: bool,
    /// Assets to analyze together in /ai multi-asset mode
    pub ai_multi_assets: Vec<String>,
    /// Cache of the previous watcher analysis detail to give the AI context.
    pub ai_last_watcher_analysis: Option<String>,
    /// Preferred AI answer language: "" = auto (match the user), "en", "ru".
    pub preferred_language: String,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            default_watchlist: vec!["BTC".into(), "ETH".into(), "LINK".into(), "SOL".into()],
            currency_display: CurrencyDisplay::Usd,
            backup_on_change: true,
            alert_quiet_hours_start: None,
            alert_quiet_hours_end: None,
            ai_model_override: None,
            ai_last_critical_notify: None,
            ai_last_important_notify: None,
            ai_watcher_enabled: true,
            ai_multi_assets: vec!["BTC".into(), "ETH".into(), "LINK".into(), "SOL".into()],
            ai_last_watcher_analysis: None,
            preferred_language: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CurrencyDisplay {
    Usd,
    Byn,
    Both,
}
