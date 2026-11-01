use crate::config::Config;
use serde::Serialize;

#[derive(Serialize)]
pub struct InputRichMessage {
    pub markdown: String,
}

#[derive(Serialize)]
pub struct SendRichMessageRequest {
    pub chat_id: i64,
    pub rich_message: InputRichMessage,
    /// Bot API 10.1+ `sendRichMessage` accepts the same `reply_markup` as
    /// `sendMessage`, so a rich table can still carry inline buttons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_markup: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub struct EditRichMessageRequest {
    pub chat_id: i64,
    pub message_id: i32,
    pub rich_message: InputRichMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_markup: Option<serde_json::Value>,
}

pub async fn send_rich_message_markdown(
    config: &Config,
    chat_id: i64,
    markdown: &str,
) -> anyhow::Result<()> {
    send_rich_message_markdown_with_markup(config, chat_id, markdown, None).await
}

pub async fn send_rich_message_markdown_with_markup(
    config: &Config,
    chat_id: i64,
    markdown: &str,
    reply_markup: Option<serde_json::Value>,
) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let url = format!(
        "https://api.telegram.org/bot{}/sendRichMessage",
        config.telegram_token
    );

    let req = SendRichMessageRequest {
        chat_id,
        rich_message: InputRichMessage {
            markdown: markdown.to_string(),
        },
        reply_markup,
    };

    let res = client.post(&url).json(&req).send().await?;

    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    // Telegram returns HTTP 200 with `{"ok":false,...}` for some rejections,
    // so a bare status check reports success on a message that never rendered.
    if !status.is_success() || !is_ok(&body) {
        return Err(anyhow::anyhow!("Failed to send rich message: {body}"));
    }

    Ok(())
}

/// True when the Bot API response body says `"ok": true`.
fn is_ok(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("ok").and_then(|ok| ok.as_bool()))
        .unwrap_or(false)
}

/// Edit an existing message into a rich (markdown) one, keeping/replacing the
/// inline keyboard. Bot API 10.1 added `rich_message` to `editMessageText`.
pub async fn edit_rich_message_markdown(
    config: &Config,
    chat_id: i64,
    message_id: i32,
    markdown: &str,
    reply_markup: Option<serde_json::Value>,
) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let url = format!(
        "https://api.telegram.org/bot{}/editMessageText",
        config.telegram_token
    );

    let req = EditRichMessageRequest {
        chat_id,
        message_id,
        rich_message: InputRichMessage {
            markdown: markdown.to_string(),
        },
        reply_markup,
    };

    let res = client.post(&url).json(&req).send().await?;

    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if !status.is_success() || !is_ok(&body) {
        return Err(anyhow::anyhow!("Failed to edit rich message: {body}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_ok;

    #[test]
    fn ok_flag_is_parsed_from_the_response_body() {
        assert!(is_ok(r#"{"ok":true,"result":{"message_id":1}}"#));
        // Whitespace must not defeat the check.
        assert!(is_ok("{ \"ok\" : true }"));
        // HTTP 200 + rejected payload: the case that used to look like success.
        assert!(!is_ok(
            r#"{"ok":false,"error_code":400,"description":"Bad Request: can't parse rich message"}"#
        ));
        assert!(!is_ok("<html>502 Bad Gateway</html>"));
    }
}
