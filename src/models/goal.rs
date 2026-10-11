use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A timestamped progress entry on a goal: what has already happened
/// (user-reported via `/goal progress`, or appended by the AI on scans).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressNote {
    pub at: DateTime<Utc>,
    pub text: String,
}

/// A proactive alert message that was actually sent for a goal — the AI sees
/// the last few on the next scan so it doesn't repeat itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentAlert {
    pub at: DateTime<Utc>,
    pub text: String,
}

/// A proactive investment goal the AI watches the market for.
///
/// The proactive worker periodically scans the market and evaluates every
/// active goal; when conditions match it sends a Telegram alert, and when a
/// goal is fulfilled (or clearly stale) it can mark it done / inactive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: u64,
    pub text: String,
    pub created_at: DateTime<Utc>,
    /// When the goal stops being relevant (e.g. end of month). None = open-ended.
    pub expires_at: Option<DateTime<Utc>>,
    pub active: bool,
    /// Last time this goal triggered a proactive alert (cooldown tracker).
    pub last_alert_at: Option<DateTime<Utc>>,
    /// Last time the user explicitly declined an AI proposal to deactivate this
    /// goal ("keep active"). Used to suppress re-proposing the same goal for a
    /// longer period than the normal alert cooldown.
    #[serde(default)]
    pub last_declined_at: Option<DateTime<Utc>>,
    /// How many alerts have fired for this goal.
    pub alert_count: u32,
    /// Marked done by the user or by the AI when the condition is met.
    pub done: bool,
    /// Temporal memory: dated progress notes (user + AI), e.g.
    /// "bought 100 USD ETH", "goal marked done — x2 reached".
    pub progress_notes: Vec<ProgressNote>,
    /// Last AI reasoning about this goal (persisted after every scan so the
    /// next check remembers what the model already decided and said).
    pub ai_summary: Option<String>,
    /// Last N alert messages actually sent for this goal (prompt memory so the
    /// model doesn't re-send the same nudge; capped at 5).
    #[serde(default)]
    pub sent_alerts: Vec<SentAlert>,
}
