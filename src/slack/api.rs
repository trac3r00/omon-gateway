use std::path::Path;

use serde_json::{json, Value};

use crate::error::OmonError;
use crate::Result;

#[derive(Clone, Debug)]
pub struct SlackWebClient {
    http: reqwest::Client,
    base: String,
    bot_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlackAuthIdentity {
    pub user_id: String,
    pub team_id: String,
    pub bot_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlackHistoryMessage {
    pub user: Option<String>,
    pub text: String,
    pub ts: String,
}

impl SlackWebClient {
    pub fn new(base: impl Into<String>, bot_token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.into().trim_end_matches('/').to_string(),
            bot_token: bot_token.into(),
        }
    }

    pub fn bot_token(&self) -> &str {
        &self.bot_token
    }

    async fn post(&self, method: &str, body: &Value) -> Result<Value> {
        self.post_with_token(method, body, &self.bot_token).await
    }

    async fn post_with_token(&self, method: &str, body: &Value, token: &str) -> Result<Value> {
        let url = format!("{}/{method}", self.base);
        let response = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .map_err(|error| OmonError::Slack(format!("{method} request failed: {error}")))?;
        let status = response.status();
        let payload: Value = response
            .json()
            .await
            .map_err(|error| OmonError::Slack(format!("{method} returned invalid JSON: {error}")))?;
        if !status.is_success() {
            return Err(OmonError::Slack(format!(
                "{method} HTTP {status}: {payload}"
            )));
        }
        if payload.get("ok").and_then(Value::as_bool) != Some(true) {
            let code = payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error");
            return Err(OmonError::Slack(format!("{method} failed: {code}")));
        }
        Ok(payload)
    }

    pub async fn auth_test(&self) -> Result<SlackAuthIdentity> {
        let payload = self.post("auth.test", &json!({})).await?;
        Ok(SlackAuthIdentity {
            user_id: required_str(&payload, "user_id")?.to_string(),
            team_id: required_str(&payload, "team_id")?.to_string(),
            bot_id: payload
                .get("bot_id")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    pub async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String> {
        let mut body = json!({"channel": channel, "text": text});
        if let Some(thread_ts) = thread_ts {
            body["thread_ts"] = json!(thread_ts);
        }
        let payload = self.post("chat.postMessage", &body).await?;
        Ok(required_str(&payload, "ts")?.to_string())
    }

    pub async fn post_message_blocks(
        &self,
        channel: &str,
        text: &str,
        blocks: Value,
        thread_ts: Option<&str>,
    ) -> Result<String> {
        let mut body = json!({"channel": channel, "text": text, "blocks": blocks});
        if let Some(thread_ts) = thread_ts {
            body["thread_ts"] = json!(thread_ts);
        }
        let payload = self.post("chat.postMessage", &body).await?;
        Ok(required_str(&payload, "ts")?.to_string())
    }

    pub async fn update_message(&self, channel: &str, ts: &str, text: &str) -> Result<()> {
        self.post(
            "chat.update",
            &json!({"channel": channel, "ts": ts, "text": text}),
        )
        .await?;
        Ok(())
    }

    pub async fn update_message_blocks(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
        blocks: Value,
    ) -> Result<()> {
        self.post(
            "chat.update",
            &json!({"channel": channel, "ts": ts, "text": text, "blocks": blocks}),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_message(&self, channel: &str, ts: &str) -> Result<()> {
        self.post("chat.delete", &json!({"channel": channel, "ts": ts}))
            .await?;
        Ok(())
    }

    pub async fn add_reaction(&self, channel: &str, ts: &str, name: &str) -> Result<()> {
        self.post(
            "reactions.add",
            &json!({"channel": channel, "timestamp": ts, "name": name}),
        )
        .await?;
        Ok(())
    }

    pub async fn remove_reaction(&self, channel: &str, ts: &str, name: &str) -> Result<()> {
        self.post(
            "reactions.remove",
            &json!({"channel": channel, "timestamp": ts, "name": name}),
        )
        .await?;
        Ok(())
    }

    pub async fn open_socket_connection(&self, app_token: &str) -> Result<String> {
        let payload = self
            .post_with_token("apps.connections.open", &json!({}), app_token)
            .await?;
        Ok(required_str(&payload, "url")?.to_string())
    }

    pub async fn history(&self, channel: &str, limit: u32) -> Result<Vec<SlackHistoryMessage>> {
        let payload = self
            .post(
                "conversations.history",
                &json!({"channel": channel, "limit": limit}),
            )
            .await?;
        parse_history_messages(&payload)
    }

    pub async fn replies(
        &self,
        channel: &str,
        thread_ts: &str,
        limit: u32,
    ) -> Result<Vec<SlackHistoryMessage>> {
        let payload = self
            .post(
                "conversations.replies",
                &json!({"channel": channel, "ts": thread_ts, "limit": limit}),
            )
            .await?;
        parse_history_messages(&payload)
    }

    pub async fn upload_file(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        path: &Path,
        initial_comment: Option<&str>,
    ) -> Result<()> {
        let bytes = tokio::fs::read(path).await.map_err(|error| {
            OmonError::Slack(format!("failed to read {}: {error}", path.display()))
        })?;
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("upload.bin")
            .to_string();

        let upload = self
            .post(
                "files.getUploadURLExternal",
                &json!({"filename": filename, "length": bytes.len()}),
            )
            .await?;
        let upload_url = required_str(&upload, "upload_url")?;
        let file_id = required_str(&upload, "file_id")?.to_string();

        let response = self
            .http
            .post(upload_url)
            .body(bytes)
            .send()
            .await
            .map_err(|error| OmonError::Slack(format!("file upload POST failed: {error}")))?;
        if !response.status().is_success() {
            return Err(OmonError::Slack(format!(
                "file upload POST returned HTTP {}",
                response.status()
            )));
        }

        let mut complete = json!({
            "files": [{"id": file_id, "title": filename}],
            "channel_id": channel,
        });
        if let Some(thread_ts) = thread_ts {
            complete["thread_ts"] = json!(thread_ts);
        }
        if let Some(comment) = initial_comment {
            complete["initial_comment"] = json!(comment);
        }
        self.post("files.completeUploadExternal", &complete).await?;
        Ok(())
    }

    pub async fn download_file(&self, url: &str) -> Result<Vec<u8>> {
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .map_err(|error| OmonError::Slack(format!("file download failed: {error}")))?;
        if !response.status().is_success() {
            return Err(OmonError::Slack(format!(
                "file download returned HTTP {}",
                response.status()
            )));
        }
        response
            .bytes()
            .await
            .map(|body| body.to_vec())
            .map_err(|error| OmonError::Slack(format!("file download body failed: {error}")))
    }
}

fn required_str<'a>(payload: &'a Value, field: &str) -> Result<&'a str> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| OmonError::Slack(format!("response missing required field {field}")))
}

fn parse_history_messages(payload: &Value) -> Result<Vec<SlackHistoryMessage>> {
    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| OmonError::Slack("history response missing messages".into()))?;
    Ok(messages
        .iter()
        .filter(|message| message.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|message| {
            Some(SlackHistoryMessage {
                user: message
                    .get("user")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                text: message.get("text").and_then(Value::as_str)?.to_string(),
                ts: message.get("ts").and_then(Value::as_str)?.to_string(),
            })
        })
        .collect())
}
