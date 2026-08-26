use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::debug;
use uuid::Uuid;

use crate::models::{OutboundAction, SessionKey, StreamChunk};
use crate::multiplexer::OutboundDispatcher;
use crate::Result;

use super::api::SlackWebClient;

pub const SLACK_MESSAGE_LIMIT: usize = 4_000;
const STREAM_DEBOUNCE: Duration = Duration::from_millis(800);

pub fn slack_emoji_name(emoji: &str) -> String {
    match emoji {
        "👀" => "eyes".to_string(),
        "✅" => "white_check_mark".to_string(),
        "❌" => "x".to_string(),
        other => other.trim_matches(':').to_string(),
    }
}

pub fn approval_blocks(request_id: Uuid, command: &str, reason: &str) -> Value {
    let button = |label: &str, style: Option<&str>, decision: &str| {
        let mut element = json!({
            "type": "button",
            "text": {"type": "plain_text", "text": label},
            "value": format!("omon:approval:{request_id}:{decision}"),
            "action_id": format!("omon_approval_{decision}"),
        });
        if let Some(style) = style {
            element["style"] = json!(style);
        }
        element
    };
    json!([
        {
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": format!("*Approval requested*\n```{command}```\n{reason}"),
            },
        },
        {
            "type": "actions",
            "elements": [
                button("Allow Once", Some("primary"), "once"),
                button("Allow Session", None, "session"),
                button("Always Allow", None, "always"),
                button("Deny", Some("danger"), "deny"),
            ],
        },
    ])
}

struct SlackStream {
    message_ts: Option<String>,
    last_edit: Option<Instant>,
}

pub struct SlackEgress {
    client: SlackWebClient,
    streams: Arc<Mutex<HashMap<String, SlackStream>>>,
    approvals: Arc<Mutex<HashMap<Uuid, (String, String)>>>,
    stream_debounce: Duration,
}

impl SlackEgress {
    pub fn new(client: SlackWebClient) -> Self {
        Self {
            client,
            streams: Arc::new(Mutex::new(HashMap::new())),
            approvals: Arc::new(Mutex::new(HashMap::new())),
            stream_debounce: STREAM_DEBOUNCE,
        }
    }

    pub fn client(&self) -> &SlackWebClient {
        &self.client
    }

    async fn stream(&self, session: &SessionKey, chunk: StreamChunk) -> Result<()> {
        let key = session.storage_key();
        if !chunk.is_final {
            let streams = self.streams.lock().await;
            if let Some(stream) = streams.get(&key) {
                if stream
                    .last_edit
                    .is_some_and(|instant| instant.elapsed() < self.stream_debounce)
                {
                    return Ok(());
                }
            }
        }

        let mut streams = self.streams.lock().await;
        let stream = streams.entry(key).or_insert(SlackStream {
            message_ts: None,
            last_edit: None,
        });
        match &stream.message_ts {
            None => {
                let ts = self
                    .client
                    .post_message(
                        &session.channel_id,
                        &chunk.content,
                        session.thread_id.as_deref(),
                    )
                    .await?;
                stream.message_ts = Some(ts);
            }
            Some(ts) => {
                let ts = ts.clone();
                self.client
                    .update_message(&session.channel_id, &ts, &chunk.content)
                    .await?;
            }
        }
        stream.last_edit = Some(Instant::now());
        Ok(())
    }
}

#[async_trait]
impl OutboundDispatcher for SlackEgress {
    async fn dispatch(&self, action: OutboundAction) -> Result<()> {
        match action {
            OutboundAction::SendMessage {
                session,
                content,
                reply_to,
            } => {
                let thread_ts = session
                    .thread_id
                    .as_deref()
                    .or(reply_to.as_deref());
                let chunks =
                    crate::discord::throttler::chunk_markdown(&content, SLACK_MESSAGE_LIMIT);
                for chunk in chunks {
                    self.client
                        .post_message(&session.channel_id, &chunk, thread_ts)
                        .await?;
                }
            }
            OutboundAction::EditMessage {
                session,
                platform_message_id,
                content,
            } => {
                self.client
                    .update_message(&session.channel_id, &platform_message_id, &content)
                    .await?;
            }
            OutboundAction::DeleteMessage {
                session,
                platform_message_id,
            } => {
                self.client
                    .delete_message(&session.channel_id, &platform_message_id)
                    .await?;
            }
            OutboundAction::UploadFile { session, path } => {
                self.client
                    .upload_file(
                        &session.channel_id,
                        session.thread_id.as_deref(),
                        &path,
                        None,
                    )
                    .await?;
            }
            OutboundAction::Stream { session, chunk } => {
                self.stream(&session, chunk).await?;
            }
            OutboundAction::Typing { session, active } => {
                debug!(
                    channel = %session.channel_id,
                    active, "slack has no bot typing indicator; ignoring typing action"
                );
            }
            OutboundAction::React {
                session,
                message_id,
                emoji,
                remove_others,
            } => {
                if remove_others {
                    let _ = self
                        .client
                        .remove_reaction(
                            &session.channel_id,
                            &message_id,
                            slack_emoji_name(crate::models::PROCESSING_START_EMOJI).as_str(),
                        )
                        .await;
                }
                self.client
                    .add_reaction(
                        &session.channel_id,
                        &message_id,
                        &slack_emoji_name(&emoji),
                    )
                    .await?;
            }
            OutboundAction::ApprovalRequest {
                session,
                request_id,
                command,
                reason,
            } => {
                let text = format!("Approval requested: {command}");
                let ts = self
                    .client
                    .post_message_blocks(
                        &session.channel_id,
                        &text,
                        approval_blocks(request_id, &command, &reason),
                        session.thread_id.as_deref(),
                    )
                    .await?;
                self.approvals
                    .lock()
                    .await
                    .insert(request_id, (session.channel_id.clone(), ts));
            }
            OutboundAction::ExpireApproval { request_id } => {
                if let Some((channel, ts)) = self.approvals.lock().await.remove(&request_id) {
                    let _ = self
                        .client
                        .update_message_blocks(
                            &channel,
                            &ts,
                            "⏱ Approval request expired",
                            json!([]),
                        )
                        .await;
                }
            }
        }
        Ok(())
    }
}
