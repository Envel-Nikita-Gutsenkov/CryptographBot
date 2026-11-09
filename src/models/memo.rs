use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A free-form personal note the AI must always remember
/// (e.g. "always invest $100 at a time", "BTC is on hold, watch alts").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memo {
    pub id: u64,
    pub text: String,
    pub created_at: DateTime<Utc>,
}
