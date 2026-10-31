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

    if !res.status().is_success() {
        let text = res.text().await?;
        return Err(anyhow::anyhow!("Failed to send rich message: {}", text));
    }

    Ok(())
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

    if !res.status().is_success() {
        let text = res.text().await?;
        return Err(anyhow::anyhow!("Failed to edit rich message: {}", text));
    }

    Ok(())
}
