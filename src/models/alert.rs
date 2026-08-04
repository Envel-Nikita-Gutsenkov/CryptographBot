use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRule {
    pub id: u64,
    pub symbol: String,
    pub condition: AlertCondition,
    pub threshold: f64,
    pub period: Option<Period>,
    pub cooldown_hours: u64,
    pub last_fired: Option<DateTime<Utc>>,
    /// Must trigger on N consecutive checks before firing (dedup).
    pub consecutive_triggers: u8,
    pub enabled: bool,
    pub label: Option<String>,
    #[serde(default)]
    pub last_fired_price: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AlertCondition {
    PriceBelow,
    PriceAbove,
    DropPercent,
    RisePercent,
}

impl std::fmt::Display for AlertCondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AlertCondition::PriceBelow => write!(f, "price ↓ below"),
            AlertCondition::PriceAbove => write!(f, "price ↑ above"),
            AlertCondition::DropPercent => write!(f, "drop % over period"),
            AlertCondition::RisePercent => write!(f, "rise % over period"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Period {
    OneHour,
    FourHours,
    OneDay,
    SevenDays,
    ThirtyDays,
    NinetyDays,
}

impl std::fmt::Display for Period {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Period::OneHour => write!(f, "1h"),
            Period::FourHours => write!(f, "4h"),
            Period::OneDay => write!(f, "24h"),
            Period::SevenDays => write!(f, "7d"),
            Period::ThirtyDays => write!(f, "30d"),
            Period::NinetyDays => write!(f, "90d"),
        }
    }
}
