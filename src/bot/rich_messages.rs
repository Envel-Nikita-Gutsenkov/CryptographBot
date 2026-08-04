use serde::Serialize;
use crate::config::Config;

#[derive(Serialize)]
pub struct InputRichMessage {
    pub markdown: String,
}

#[derive(Serialize)]
pub struct SendRichMessageRequest {
    pub chat_id: i64,
    pub rich_message: InputRichMessage,
}

pub async fn send_rich_message_markdown(
    config: &Config,
    chat_id: i64,
    markdown: &str,
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
    };

    let res = client.post(&url)
        .json(&req)
        .send()
        .await?;

    if !res.status().is_success() {
        let text = res.text().await?;
        return Err(anyhow::anyhow!("Failed to send rich message: {}", text));
    }

    Ok(())
}
