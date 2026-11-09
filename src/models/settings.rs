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
    /// Canonical security topic (e.g. "binance-breach") of the last security
    /// alert actually delivered. Lets the watcher notify on the FIRST sighting
    /// of an incident and then stay quiet about the same incident for days.
    #[serde(default)]
    pub ai_last_security_topic: Option<String>,
    #[serde(default)]
    pub ai_last_security_notify: Option<DateTime<Utc>>,
    /// Every security topic already delivered, with its delivery time. A single
    /// "last topic" slot caused repeated alerts: when two incidents alternated
    /// in the feed (`binance`, then `theft`, then `binance` again) the same
    /// breach was re-sent every scan because the *previous* topic differed.
    /// Topics older than the cooldown window are pruned on write.
    #[serde(default)]
    pub ai_security_seen: std::collections::HashMap<String, DateTime<Utc>>,
    /// Proactive dip-zone watcher. Enabled by default for buy-the-dip users;
    /// it only fires when the price is at/below the average entry AND inside a
    /// dip zone, so it is not a general price-spam channel.
    #[serde(default = "default_true")]
    pub dip_alert_enabled: bool,
    /// Per-zone cooldown for the proactive dip alert.
    #[serde(default = "default_dip_cooldown_hours")]
    pub dip_cooldown_hours: u64,
    /// `symbol:zone_key` → last delivery time. A map (not a single slot) so
    /// alternating symbols/zones cannot re-send the same alert — the exact
    /// regression that caused the security-alert spam.
    #[serde(default)]
    pub dip_seen: std::collections::HashMap<String, DateTime<Utc>>,
    /// "Hardcore Dip Sniper": cash parked in stablecoins, waiting for a
    /// left-tail anomaly. One pool shared across symbols.
    #[serde(default)]
    pub sniper_cash_usdt: f64,
    /// Warn when a bucket fill would raise the average entry by >10% while the
    /// position is already in strong profit.
    #[serde(default = "default_true")]
    pub sniper_protect_avg_entry: bool,
    /// Daily bucket/capitulation pings for the sniper.
    #[serde(default = "default_true")]
    pub sniper_alerts_enabled: bool,
    /// `symbol:event` → last delivery time. Map, not a single slot, so
    /// alternating symbols/buckets can never re-send the same event.
    #[serde(default)]
    pub sniper_seen: std::collections::HashMap<String, DateTime<Utc>>,
}

fn default_true() -> bool {
    true
}

fn default_dip_cooldown_hours() -> u64 {
    168
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
            ai_last_security_topic: None,
            ai_last_security_notify: None,
            ai_security_seen: std::collections::HashMap::new(),
            dip_alert_enabled: true,
            dip_cooldown_hours: 168,
            dip_seen: std::collections::HashMap::new(),
            sniper_cash_usdt: 0.0,
            sniper_protect_avg_entry: true,
            sniper_alerts_enabled: true,
            sniper_seen: std::collections::HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CurrencyDisplay {
    Usd,
    Byn,
    Both,
}
