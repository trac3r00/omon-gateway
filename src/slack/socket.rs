use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::error::OmonError;
use crate::Result;

use super::api::SlackWebClient;

const SEEN_ENVELOPE_CAPACITY: usize = 1024;
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub enum SocketEvent {
    EventsApi { envelope_id: String, payload: Value },
    Interactive { envelope_id: String, payload: Value },
    SlashCommand { envelope_id: String, payload: Value },
}

pub struct SocketModeClient {
    api: SlackWebClient,
    app_token: String,
    socket_url_override: Option<String>,
}

struct SeenEnvelopes {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl SeenEnvelopes {
    fn new() -> Self {
        Self {
            order: VecDeque::new(),
            set: HashSet::new(),
        }
    }

    fn insert_if_new(&mut self, envelope_id: &str) -> bool {
        if !self.set.insert(envelope_id.to_string()) {
            return false;
        }
        self.order.push_back(envelope_id.to_string());
        while self.order.len() > SEEN_ENVELOPE_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
        true
    }
}

impl SocketModeClient {
    pub fn new(api: SlackWebClient, app_token: impl Into<String>) -> Self {
        Self {
            api,
            app_token: app_token.into(),
            socket_url_override: None,
        }
    }

    pub fn with_socket_url_override(mut self, url: impl Into<String>) -> Self {
        self.socket_url_override = Some(url.into());
        self
    }

    pub async fn run(
        &self,
        tx: mpsc::Sender<SocketEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let mut backoff = INITIAL_BACKOFF;
        let mut seen = SeenEnvelopes::new();
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            match self.connect_once(&tx, &cancel, &mut seen).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    warn!(%error, ?backoff, "slack socket connection lost; reconnecting");
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = cancel.cancelled() => return Ok(()),
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    async fn connect_once(
        &self,
        tx: &mpsc::Sender<SocketEvent>,
        cancel: &CancellationToken,
        seen: &mut SeenEnvelopes,
    ) -> Result<()> {
        let url = match &self.socket_url_override {
            Some(url) => url.clone(),
            None => self.api.open_socket_connection(&self.app_token).await?,
        };
        let (mut ws, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|error| OmonError::Slack(format!("socket mode connect failed: {error}")))?;
        info!("slack socket mode connected");

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = ws.close(None).await;
                    return Ok(());
                }
                frame = ws.next() => {
                    let frame = frame
                        .ok_or_else(|| OmonError::Slack("socket closed by peer".into()))?
                        .map_err(|error| OmonError::Slack(format!("socket read failed: {error}")))?;
                    match frame {
                        Message::Text(text) => {
                            if let Ok(value) = serde_json::from_str::<Value>(text.as_str()) {
                                if let Some(envelope_id) =
                                    value.get("envelope_id").and_then(Value::as_str)
                                {
                                    let _ = ws.send(ack_frame(envelope_id)).await;
                                }
                            }
                            if self.handle_frame(text.as_str(), tx, seen).await? {
                                return Err(OmonError::Slack(
                                    "slack requested socket reconnect".into(),
                                ));
                            }
                        }
                        Message::Ping(payload) => {
                            let _ = ws.send(Message::Pong(payload)).await;
                        }
                        Message::Close(_) => {
                            return Err(OmonError::Slack("socket close frame received".into()));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn handle_frame(
        &self,
        text: &str,
        tx: &mpsc::Sender<SocketEvent>,
        seen: &mut SeenEnvelopes,
    ) -> Result<bool> {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            debug!(frame = %text, "ignoring non-JSON socket frame");
            return Ok(false);
        };
        let frame_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        match frame_type {
            "hello" => {
                info!("slack socket mode hello received");
                Ok(false)
            }
            "disconnect" => Ok(true),
            "events_api" | "interactive" | "slash_commands" => {
                let Some(envelope_id) = value.get("envelope_id").and_then(Value::as_str) else {
                    return Ok(false);
                };
                if !seen.insert_if_new(envelope_id) {
                    debug!(envelope_id, "skipping duplicate socket envelope");
                    return Ok(false);
                }
                let payload = value.get("payload").cloned().unwrap_or(Value::Null);
                let event = match frame_type {
                    "events_api" => SocketEvent::EventsApi {
                        envelope_id: envelope_id.to_string(),
                        payload,
                    },
                    "interactive" => SocketEvent::Interactive {
                        envelope_id: envelope_id.to_string(),
                        payload,
                    },
                    _ => SocketEvent::SlashCommand {
                        envelope_id: envelope_id.to_string(),
                        payload,
                    },
                };
                if tx.send(event).await.is_err() {
                    return Err(OmonError::Slack("socket event consumer dropped".into()));
                }
                Ok(false)
            }
            other => {
                debug!(frame_type = %other, "ignoring unknown socket frame type");
                Ok(false)
            }
        }
    }
}

pub fn ack_frame(envelope_id: &str) -> Message {
    Message::Text(json!({"envelope_id": envelope_id}).to_string().into())
}
