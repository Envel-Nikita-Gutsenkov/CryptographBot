//! A stored market signal: a news headline, an exchange announcement, a
//! trending-search hit or a community sentiment reading.
//!
//! Signals are the "what the crowd sees and feels" layer: they are fetched
//! from free sources, kept for a short TTL and handed to the AI only when a
//! question actually needs them. Raw rows age out; the AI's distilled take is
//! what survives in `news_summaries`.

#[derive(Debug, Clone, Default)]
pub struct MarketSignal {
    pub symbol: String,
    pub kind: String,
    pub source: String,
    pub title: String,
    pub body: String,
    pub url: String,
    /// Sentiment up-percentage, trending position, … — meaning depends on `kind`.
    pub score: Option<f64>,
    pub published_at: Option<String>,
}

impl MarketSignal {
    pub const KIND_NEWS: &'static str = "news";
    pub const KIND_ANNOUNCEMENT: &'static str = "announcement";
    pub const KIND_TRENDING: &'static str = "trending";
    pub const KIND_SENTIMENT: &'static str = "sentiment";

    pub fn new(kind: &str, source: &str, symbol: &str, title: impl Into<String>) -> Self {
        Self {
            symbol: symbol.to_uppercase(),
            kind: kind.to_string(),
            source: source.to_string(),
            title: title.into(),
            ..Default::default()
        }
    }

    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    pub fn with_score(mut self, score: f64) -> Self {
        self.score = Some(score);
        self
    }

    pub fn with_published_at(mut self, at: impl Into<String>) -> Self {
        self.published_at = Some(at.into());
        self
    }
}
