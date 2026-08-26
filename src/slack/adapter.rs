use crate::models::{InboundEvent, MessageAttachment, SessionKey};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlackChannelType {
    Im,
    Channel,
    Group,
}

impl SlackChannelType {
    pub fn is_dm(self) -> bool {
        matches!(self, Self::Im)
    }
}

#[derive(Clone, Debug)]
pub struct SlackMessageEvent {
    pub event_id: String,
    pub team_id: String,
    pub channel: String,
    pub channel_type: SlackChannelType,
    pub user: String,
    pub text: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub bot_id: Option<String>,
    pub files: Vec<MessageAttachment>,
    pub mentions_bot: bool,
}

impl SlackMessageEvent {
    fn is_dm(&self) -> bool {
        self.channel_type.is_dm()
    }

    fn thread_anchor(&self) -> Option<&str> {
        self.thread_ts.as_deref()
    }
}

#[derive(Clone, Debug, Default)]
pub struct SlackInboundFilter<'a> {
    pub free_response_channels: &'a [String],
    pub allowed_users: &'a [String],
    pub allow_all_users: bool,
    pub thread_sessions_per_user: bool,
    pub active_threads: &'a [String],
    pub allowed_channels: &'a [String],
    pub ignored_channels: &'a [String],
    pub thread_require_mention: bool,
    pub paired_users: &'a [String],
    pub bot_user_id: &'a str,
}

pub fn strip_bot_mention(text: &str, bot_user_id: &str) -> String {
    text.replace(&format!("<@{bot_user_id}>"), "").trim().to_owned()
}

pub fn event_to_inbound(
    event: &SlackMessageEvent,
    filter: &SlackInboundFilter<'_>,
) -> Option<InboundEvent> {
    if event.bot_id.is_some() || event.user == filter.bot_user_id {
        return None;
    }

    let is_dm = event.is_dm();

    if filter.ignored_channels.iter().any(|c| c == &event.channel) {
        return None;
    }
    if !is_dm
        && !filter.allowed_channels.is_empty()
        && !filter.allowed_channels.iter().any(|c| c == &event.channel)
    {
        return None;
    }

    let is_paired = filter.paired_users.iter().any(|u| u == &event.user);
    if !is_paired
        && !filter.allow_all_users
        && !filter.allowed_users.iter().any(|u| u == &event.user)
    {
        return None;
    }

    let mentions_other_bot = !event.mentions_bot && event.text.contains("<@");
    if mentions_other_bot {
        return None;
    }
    if !event.mentions_bot {
        let thread_anchor = event.thread_anchor();
        let is_active_thread = thread_anchor.is_some_and(|anchor| {
            !filter.thread_require_mention
                && filter.active_threads.iter().any(|t| t == anchor)
        });
        let is_free_channel = filter
            .free_response_channels
            .iter()
            .any(|c| c == &event.channel);
        if !is_dm && !is_active_thread && !is_free_channel {
            return None;
        }
    }

    let content = strip_bot_mention(&event.text, filter.bot_user_id);
    if content.is_empty() && event.files.is_empty() {
        return None;
    }

    let thread_id = if is_dm {
        event.thread_ts.clone()
    } else {
        Some(
            event
                .thread_ts
                .clone()
                .unwrap_or_else(|| event.ts.clone()),
        )
    };
    let user_id = if !is_dm && !filter.thread_sessions_per_user {
        "shared".to_owned()
    } else {
        event.user.clone()
    };

    let session = SessionKey::new(
        "slack",
        Some(event.team_id.clone()),
        event.channel.clone(),
        thread_id,
        user_id,
    )
    .with_bot_id(filter.bot_user_id);
    let mut inbound = InboundEvent::message(session, event.ts.clone(), content)
        .with_attachments(event.files.clone());
    inbound.delivery_id = Some(format!("slack:{}", event.event_id));
    Some(inbound)
}
