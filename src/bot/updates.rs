//! Polling listener with a repair pass for updates teloxide 0.12 cannot
//! deserialize.
//!
//! Rich messages (`sendRichMessage`) carry no `text` field and have no
//! `rich_message` counterpart in teloxide 0.12, so any update containing one
//! deserializes into `UpdateKind::Error` and is dropped by the dispatcher.
//! Replies to forecast/alert cards and button presses on them are affected.
//!
//! `repair_update` flattens `rich_message` blocks into plain text and retries the
//! parse once; updates that still fail pass through unchanged.

use futures::StreamExt;
use serde_json::{Map, Value};
use teloxide::{
    types::{AllowedUpdate, Update, UpdateKind},
    update_listeners::{
        polling_default, AsUpdateStream, Polling, StatefulListener, UpdateListener,
    },
    Bot, RequestError,
};

/// Polling listener with the rich-message repair pass attached.
pub async fn lenient_listener(bot: Bot) -> impl UpdateListener<Err = RequestError> {
    let polling = polling_default(bot).await;
    StatefulListener::new_with_hints(
        polling,
        stream_with_repair,
        |p: &mut Polling<Bot>| p.stop_token(),
        Some(
            |p: &mut Polling<Bot>, hint: &mut dyn Iterator<Item = AllowedUpdate>| {
                p.hint_allowed_updates(hint)
            },
        ),
        Some(|p: &Polling<Bot>| p.timeout_hint()),
    )
}

/// Attach the repair pass to the polling stream. A named function (not a
/// closure) keeps the higher-ranked lifetime over the borrowed listener
/// inferable.
fn stream_with_repair(
    polling: &mut Polling<Bot>,
) -> impl futures::Stream<Item = Result<Update, RequestError>> + '_ {
    polling.as_stream().map(|item| item.map(repair_update))
}

/// Re-parse an update teloxide could not deserialize.
///
/// Parsable updates pass through untouched. An `Error` variant carries the raw
/// update JSON; the repair is one retry on a sanitized copy. If the retry fails
/// too, the original value is returned so the dispatcher still logs it.
fn repair_update(update: Update) -> Update {
    let UpdateKind::Error(raw) = &update.kind else {
        return update;
    };
    match parse_sanitized(raw) {
        Some(repaired) => repaired,
        None => update,
    }
}

fn parse_sanitized(raw: &Value) -> Option<Update> {
    let value = sanitize_value(raw.clone());
    if value == *raw {
        return None;
    }
    // JSON text, not `from_value`: teloxide's flattened `UpdateKind` only
    // deserializes from a stream and yields `Error` from a `Value`.
    let text = serde_json::to_string(&value).ok()?;
    match serde_json::from_str::<Update>(&text) {
        Ok(update) if !matches!(update.kind, UpdateKind::Error(_)) => Some(update),
        _ => None,
    }
}

/// Recursively replace every `rich_message` object with a plain `text` field.
fn sanitize_value(mut value: Value) -> Value {
    match &mut value {
        Value::Object(map) => {
            flatten_rich_message(map);
            for child in map.values_mut() {
                let child_value = std::mem::take(child);
                *child = sanitize_value(child_value);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                let item_value = std::mem::take(item);
                *item = sanitize_value(item_value);
            }
        }
        _ => {}
    }
    value
}

fn flatten_rich_message(map: &mut Map<String, Value>) {
    let Some(Value::Object(rich)) = map.get("rich_message").cloned() else {
        return;
    };
    map.remove("rich_message");
    // Keep any text/caption the client already attached; only fill the gap.
    if map.contains_key("text") || map.contains_key("caption") {
        return;
    }
    let text = rich_message_text(&rich).unwrap_or_else(|| "[rich message]".to_string());
    map.insert("text".to_string(), Value::String(text));
}

/// Flatten a rich message into readable plain text, one paragraph per line.
fn rich_message_text(rich: &Map<String, Value>) -> Option<String> {
    let blocks = rich.get("blocks")?.as_array()?;
    let mut out = String::new();
    for block in blocks {
        let mut line = String::new();
        match block.get("text") {
            Some(text) => collect_fragments(text, &mut line),
            None => collect_fragments(block, &mut line),
        }
        let line = line.trim();
        if !line.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(line);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Pull the visible strings out of a rich-text fragment tree.
///
/// Text is stored as either a bare string or an object `{"text": …, "type":
/// "bold"}`. Unknown block shapes (tables and whatever else Telegram adds later)
/// fall back to a deep walk, so a new block type degrades to its strings instead
/// of dropping the whole message.
fn collect_fragments(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => out.push_str(s),
        Value::Array(items) => {
            for item in items {
                collect_fragments(item, out);
            }
        }
        Value::Object(map) => {
            if let Some(text) = map.get("text") {
                collect_fragments(text, out);
            } else {
                for (key, child) in map {
                    if key == "type" {
                        continue;
                    }
                    collect_fragments(child, out);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
