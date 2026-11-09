use super::*;

// All fixture data below is synthetic. Never paste production chat ids, user
// names or real message bodies into tests — the repository is public.

const CHAT_ID: i64 = 1_000_001;
const BOT_ID: i64 = 1_000_002;
const MESSAGE_ID: i32 = 10;
const ALERT_MESSAGE_ID: i32 = 9;
const DATE: i64 = 1_700_000_000;
const ALERT_BODY: &str = "SOL fell 20.4% over 30 days.";
const ALERT_TITLE: &str = "Smart alert for your goals";
const ALERT_DATE: &str = "2026-11-08";

fn chat() -> Value {
    serde_json::json!({"id": CHAT_ID, "type": "private", "first_name": "Tester"})
}

fn user() -> Value {
    serde_json::json!({"id": CHAT_ID, "is_bot": false, "first_name": "Tester"})
}

fn bot() -> Value {
    serde_json::json!({"id": BOT_ID, "is_bot": true, "first_name": "Cryptograph"})
}

fn plain(text: &str) -> Value {
    Value::String(text.to_string())
}

fn bold(text: &str) -> Value {
    serde_json::json!({"type": "bold", "text": text})
}

fn paragraph(fragments: Vec<Value>) -> Value {
    serde_json::json!({"type": "paragraph", "text": fragments})
}

/// Synthetic Telegram message update, assembled field by field so a test reads
/// as "a command replying to a rich alert" instead of a wall of literals.
struct UpdateFixture {
    update_id: i32,
    text: String,
    reply_blocks: Vec<Value>,
}

impl UpdateFixture {
    fn new(text: &str) -> Self {
        Self {
            update_id: 1,
            text: text.to_string(),
            reply_blocks: Vec::new(),
        }
    }

    fn replying_to_rich_message(mut self, blocks: Vec<Value>) -> Self {
        self.reply_blocks = blocks;
        self
    }

    fn to_json(&self) -> Value {
        let mut message = serde_json::json!({
            "message_id": MESSAGE_ID,
            "date": DATE,
            "chat": chat(),
            "from": user(),
            "entities": [{"offset": 0, "length": 4, "type": "bot_command"}],
            "text": self.text,
        });
        if !self.reply_blocks.is_empty() {
            message["reply_to_message"] = serde_json::json!({
                "message_id": ALERT_MESSAGE_ID,
                "date": DATE - 60,
                "chat": chat(),
                "from": bot(),
                "rich_message": {"blocks": self.reply_blocks},
            });
        }
        serde_json::json!({"update_id": self.update_id, "message": message})
    }
}

/// Build the `UpdateKind::Error` teloxide-core produces when a nested message
/// fails to parse (`net::request::deserialize_response`): the raw update is kept
/// in the error variant. `serde_json::from_value` would keep an empty error value
/// instead, so the fixture mirrors the response layer rather than a plain parse.
fn errored_update(raw: &Value) -> Update {
    let id = raw.get("update_id").and_then(Value::as_i64).unwrap() as i32;
    Update {
        id,
        kind: UpdateKind::Error(raw.clone()),
    }
}

fn alert_update() -> Value {
    UpdateFixture::new("/ask should I buy now or wait")
        .replying_to_rich_message(vec![
            paragraph(vec![
                plain("🤖 "),
                bold(ALERT_TITLE),
                plain(&format!(" ({ALERT_DATE})")),
            ]),
            paragraph(vec![plain(ALERT_BODY)]),
        ])
        .to_json()
}

#[test]
fn production_alert_reply_now_parses() {
    let repaired = repair_update(errored_update(&alert_update()));
    let UpdateKind::Message(message) = &repaired.kind else {
        panic!("repaired update must be a message, got {:?}", repaired.kind);
    };
    assert_eq!(message.chat.id.0, CHAT_ID);
    assert_eq!(message.text(), Some("/ask should I buy now or wait"));

    let reply = message.reply_to_message().expect("reply is preserved");
    assert_eq!(
        reply.text(),
        Some(format!("🤖 {ALERT_TITLE} ({ALERT_DATE})\n\n{ALERT_BODY}").as_str())
    );
    assert!(reply.reply_markup().is_none());
}

#[test]
fn repaired_alert_reply_feeds_ask_reply_context() {
    let repaired = repair_update(errored_update(&alert_update()));
    let UpdateKind::Message(message) = &repaired.kind else {
        panic!("expected message");
    };
    let context = crate::bot::handlers::ask::reply_context_from(message)
        .expect("reply to a rich alert must produce context");
    assert!(context.starts_with("[assistant (bot)]: "));
    assert!(context.contains(ALERT_TITLE));
}

#[test]
fn valid_updates_are_untouched() {
    let raw = UpdateFixture::new("hello").to_json();
    let update: Update = serde_json::from_value(raw).unwrap();
    let repaired = repair_update(update.clone());
    assert_eq!(repaired, update);
}

#[test]
fn unknown_rich_block_shapes_degrade_to_their_strings() {
    let rich = serde_json::json!({
        "blocks": [
            {"type": "table", "rows": [["a", "b"], [{"type": "bold", "text": "c"}]]}
        ]
    })
    .as_object()
    .unwrap()
    .clone();
    let text = rich_message_text(&rich).unwrap();
    assert!(text.contains('a') && text.contains('b') && text.contains('c'));
    // Structural keywords must not leak into the text.
    assert!(!text.contains("table"));
}

#[test]
fn existing_text_is_not_overwritten() {
    let mut map = serde_json::json!({
        "text": "original",
        "rich_message": {"blocks": [{"type": "paragraph", "text": ["ignored"]}]}
    })
    .as_object()
    .unwrap()
    .clone();
    flatten_rich_message(&mut map);
    assert_eq!(map.get("text").unwrap(), "original");
    assert!(!map.contains_key("rich_message"));
}
