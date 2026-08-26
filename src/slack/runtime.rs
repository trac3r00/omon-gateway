use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::discord::approval::{
    is_approval_custom_id, parse_custom_id, ApprovalDecision, SmartApprovalGuard,
};
use crate::discord::attachments::{is_text_attachment, MAX_INLINED_ATTACHMENT_BYTES};
use crate::error::OmonError;
use crate::models::MessageAttachment;
use crate::multiplexer::{OutboundDispatcher, SessionMultiplexer};
use crate::Result;

use super::adapter::{
    event_to_inbound, SlackChannelType, SlackInboundFilter, SlackMessageEvent,
};
use super::api::{SlackAuthIdentity, SlackWebClient};
use super::egress::{slack_emoji_name, SlackEgress};
use super::pairing::{SlackPairingOutcome, SlackPairingStore};
use super::socket::{SocketEvent, SocketModeClient};

#[derive(Clone, Debug, Default)]
pub struct OwnedSlackFilter {
    pub free_response_channels: Vec<String>,
    pub allowed_users: Vec<String>,
    pub allow_all_users: bool,
    pub thread_sessions_per_user: bool,
    pub allowed_channels: Vec<String>,
    pub ignored_channels: Vec<String>,
    pub thread_require_mention: bool,
}

impl OwnedSlackFilter {
    fn borrow<'a>(
        &'a self,
        active_threads: &'a [String],
        paired_users: &'a [String],
        bot_user_id: &'a str,
    ) -> SlackInboundFilter<'a> {
        SlackInboundFilter {
            free_response_channels: &self.free_response_channels,
            allowed_users: &self.allowed_users,
            allow_all_users: self.allow_all_users,
            thread_sessions_per_user: self.thread_sessions_per_user,
            active_threads,
            allowed_channels: &self.allowed_channels,
            ignored_channels: &self.ignored_channels,
            thread_require_mention: self.thread_require_mention,
            paired_users,
            bot_user_id,
        }
    }

    fn is_authorized(&self, user_id: &str, paired_users: &[String]) -> bool {
        paired_users.iter().any(|u| u == user_id)
            || self.allow_all_users
            || self.allowed_users.iter().any(|u| u == user_id)
    }
}

#[derive(Clone, Debug)]
pub struct SlackRuntimeConfig {
    pub bot_token: String,
    pub app_token: String,
    pub api_base: String,
    pub filter: OwnedSlackFilter,
    pub processing_reactions: bool,
    pub workspace_root: PathBuf,
}

pub struct SlackRuntime {
    client: SlackWebClient,
    app_token: String,
    egress: Arc<SlackEgress>,
    filter: OwnedSlackFilter,
    processing_reactions: bool,
    workspace_root: PathBuf,
    approval_guard: SmartApprovalGuard,
    pairing: SlackPairingStore,
    active_threads: Arc<RwLock<HashSet<String>>>,
    multiplexer: Option<SessionMultiplexer>,
}

impl SlackRuntime {
    pub fn new(
        config: SlackRuntimeConfig,
        approval_guard: SmartApprovalGuard,
        pairing: SlackPairingStore,
    ) -> Self {
        let client = SlackWebClient::new(config.api_base, config.bot_token);
        let egress = Arc::new(SlackEgress::new(client.clone()));
        Self {
            client,
            app_token: config.app_token,
            egress,
            filter: config.filter,
            processing_reactions: config.processing_reactions,
            workspace_root: config.workspace_root,
            approval_guard,
            pairing,
            active_threads: Arc::new(RwLock::new(HashSet::new())),
            multiplexer: None,
        }
    }

    pub fn egress_dispatcher(&self) -> Arc<dyn OutboundDispatcher> {
        self.egress.clone()
    }

    pub fn set_multiplexer(&mut self, multiplexer: SessionMultiplexer) {
        self.multiplexer = Some(multiplexer);
    }

    pub async fn run(mut self, cancel: CancellationToken) -> Result<()> {
        let identity = self.client.auth_test().await?;
        info!(
            bot_user_id = %identity.user_id,
            team_id = %identity.team_id,
            "slack identity resolved via auth.test"
        );
        self.pairing.init_cache().await?;

        let multiplexer = self
            .multiplexer
            .take()
            .ok_or_else(|| OmonError::Config("slack runtime missing multiplexer".into()))?;

        let socket = SocketModeClient::new(self.client.clone(), self.app_token.clone());
        let (tx, mut rx) = mpsc::channel::<SocketEvent>(256);
        let socket_cancel = cancel.clone();
        let socket_task = tokio::spawn(async move { socket.run(tx, socket_cancel).await });

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                maybe_event = rx.recv() => {
                    match maybe_event {
                        Some(SocketEvent::EventsApi { payload, .. }) => {
                            if let Err(error) =
                                self.handle_events_api(&payload, &identity, &multiplexer).await
                            {
                                warn!(%error, "slack events_api handling failed");
                            }
                        }
                        Some(SocketEvent::Interactive { payload, .. }) => {
                            if let Err(error) = self.handle_interactive(&payload).await {
                                warn!(%error, "slack interactive handling failed");
                            }
                        }
                        Some(SocketEvent::SlashCommand { .. }) => {
                            debug!("slack slash commands are not supported; ignoring");
                        }
                        None => break,
                    }
                }
            }
        }

        cancel.cancel();
        match socket_task.await {
            Ok(result) => result?,
            Err(error) => {
                return Err(OmonError::Slack(format!("socket task join failed: {error}")))
            }
        }
        Ok(())
    }

    async fn handle_events_api(
        &self,
        payload: &Value,
        identity: &SlackAuthIdentity,
        multiplexer: &SessionMultiplexer,
    ) -> Result<()> {
        let event = payload.get("event").cloned().unwrap_or(Value::Null);
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(kind, "message" | "app_mention") {
            return Ok(());
        }
        if kind == "message" {
            let subtype = event.get("subtype").and_then(Value::as_str);
            if !matches!(subtype, None | Some("thread_broadcast") | Some("file_share")) {
                return Ok(());
            }
        }

        let Some(user) = event.get("user").and_then(Value::as_str) else {
            return Ok(());
        };
        let Some(channel) = event.get("channel").and_then(Value::as_str) else {
            return Ok(());
        };
        let Some(ts) = event.get("ts").and_then(Value::as_str) else {
            return Ok(());
        };
        let text = event
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let channel_type = match event.get("channel_type").and_then(Value::as_str) {
            Some("im") => SlackChannelType::Im,
            Some("group") => SlackChannelType::Group,
            _ => SlackChannelType::Channel,
        };
        let is_dm = channel_type.is_dm();
        let paired_users = self.pairing.get_paired_user_ids_sync();
        let authorized = self.filter.is_authorized(user, &paired_users);

        if is_dm && !authorized {
            self.handle_unauthorized_dm(channel, user, &text).await?;
            return Ok(());
        }
        if is_dm && authorized {
            if let Some(code) = text.trim().strip_prefix("approve ") {
                return self.handle_operator_approval(channel, code).await;
            }
        }

        let files = self.resolve_files(&event).await;
        let bot_tag = format!("<@{}>", identity.user_id);
        let slack_event = SlackMessageEvent {
            event_id: payload
                .get("event_id")
                .and_then(Value::as_str)
                .unwrap_or(ts)
                .to_string(),
            team_id: payload
                .get("team_id")
                .and_then(Value::as_str)
                .unwrap_or(&identity.team_id)
                .to_string(),
            channel: channel.to_string(),
            channel_type,
            user: user.to_string(),
            text: text.clone(),
            ts: ts.to_string(),
            thread_ts: event
                .get("thread_ts")
                .and_then(Value::as_str)
                .map(str::to_string),
            bot_id: event
                .get("bot_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            files,
            mentions_bot: kind == "app_mention" || text.contains(&bot_tag),
        };

        let active: Vec<String> = self.active_threads.read().await.iter().cloned().collect();
        let filter = self
            .filter
            .borrow(&active, &paired_users, &identity.user_id);
        let Some(inbound) = event_to_inbound(&slack_event, &filter) else {
            return Ok(());
        };

        if self.processing_reactions {
            if let Err(error) = self
                .client
                .add_reaction(channel, ts, &slack_emoji_name(crate::models::PROCESSING_START_EMOJI))
                .await
            {
                debug!(%error, channel, ts, "failed to add processing reaction");
            }
        }
        if !is_dm {
            if let Some(anchor) = &inbound.session.thread_id {
                self.active_threads.write().await.insert(anchor.clone());
            }
        }
        multiplexer.route(inbound).await
    }

    async fn handle_unauthorized_dm(&self, channel: &str, user: &str, text: &str) -> Result<()> {
        let normalized = crate::discord::pairing::PairingStore::normalize_code(text);
        if !normalized.is_empty() && normalized.len() == 8 {
            if let Ok(SlackPairingOutcome::Success { user_id }) =
                self.pairing.approve_code(text).await
            {
                self.client
                    .post_message(
                        channel,
                        &format!("✅ Pairing approved for <@{user_id}>. You can talk to me now."),
                        None,
                    )
                    .await?;
                return Ok(());
            }
        }
        let code = self.pairing.request_pairing_code(user).await?;
        self.client
            .post_message(
                channel,
                &format!(
                    "You are not authorized yet. Your pairing code is `{code}` — an operator can approve it by DMing me `approve {code}`."
                ),
                None,
            )
            .await?;
        Ok(())
    }

    async fn handle_operator_approval(&self, channel: &str, code: &str) -> Result<()> {
        let reply = match self.pairing.approve_code(code).await? {
            SlackPairingOutcome::Success { user_id } => {
                format!("✅ Pairing approved for <@{user_id}>.")
            }
            SlackPairingOutcome::InvalidCode => "❌ Invalid pairing code.".to_string(),
            SlackPairingOutcome::Expired => "❌ That pairing code has expired.".to_string(),
            SlackPairingOutcome::LockedOut => {
                "❌ That pairing code is locked after too many attempts.".to_string()
            }
        };
        self.client.post_message(channel, &reply, None).await?;
        Ok(())
    }

    async fn handle_interactive(&self, payload: &Value) -> Result<()> {
        if payload.get("type").and_then(Value::as_str) != Some("block_actions") {
            return Ok(());
        }
        let user = payload
            .get("user")
            .and_then(|u| u.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let channel = payload
            .get("channel")
            .and_then(|c| c.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let message_ts = payload
            .get("message")
            .and_then(|m| m.get("ts"))
            .and_then(Value::as_str)
            .unwrap_or_default();

        let paired_users = self.pairing.get_paired_user_ids_sync();
        if !self.filter.is_authorized(user, &paired_users) {
            warn!(user, "unauthorized slack approval click ignored");
            return Ok(());
        }

        let actions = payload
            .get("actions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for action in actions {
            let value = action.get("value").and_then(Value::as_str).unwrap_or("");
            if !is_approval_custom_id(value) {
                continue;
            }
            let label = if self.approval_guard.resolve_custom_id(value).await {
                match parse_custom_id(value).map(|(_, decision)| decision) {
                    Some(ApprovalDecision::Once) => "Approved (once)",
                    Some(ApprovalDecision::Session) => "Approved (session)",
                    Some(ApprovalDecision::Always) => "Approved (always)",
                    Some(ApprovalDecision::Deny { .. }) => "Denied",
                    None => "Resolved",
                }
            } else {
                "⏱ Approval request no longer valid (expired or already resolved)."
            };
            if !channel.is_empty() && !message_ts.is_empty() {
                self.client
                    .update_message_blocks(
                        channel,
                        message_ts,
                        &format!("{label} by <@{user}>"),
                        serde_json::json!([]),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn resolve_files(&self, event: &Value) -> Vec<MessageAttachment> {
        let files = event
            .get("files")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut attachments = Vec::new();
        for file in files {
            let id = file
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let filename = file
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("file")
                .to_string();
            let url = file
                .get("url_private")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let content_type = file
                .get("mimetype")
                .and_then(Value::as_str)
                .map(str::to_string);
            let size_bytes = file.get("size").and_then(Value::as_u64);

            let mut attachment = MessageAttachment {
                id,
                filename,
                url,
                content_type,
                size_bytes,
                local_path: None,
                text_content: None,
            };

            let small_enough = size_bytes.is_some_and(|size| {
                size > 0 && size <= MAX_INLINED_ATTACHMENT_BYTES
            });
            if small_enough
                && !attachment.url.is_empty()
                && is_text_attachment(&attachment.filename, attachment.content_type.as_deref())
            {
                match self.client.download_file(&attachment.url).await {
                    Ok(bytes) => {
                        attachment.text_content =
                            Some(String::from_utf8_lossy(&bytes).into_owned());
                        let dir = self.workspace_root.join("attachments");
                        let path = dir.join(format!("{}-{}", attachment.id, attachment.filename));
                        if tokio::fs::create_dir_all(&dir).await.is_ok()
                            && tokio::fs::write(&path, &bytes).await.is_ok()
                        {
                            attachment.local_path = Some(path);
                        }
                    }
                    Err(error) => {
                        debug!(%error, file = %attachment.filename, "slack attachment download failed");
                    }
                }
            }
            attachments.push(attachment);
        }
        attachments
    }
}
