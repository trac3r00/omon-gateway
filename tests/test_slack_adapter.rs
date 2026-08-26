use omon_gateway::slack::{
    event_to_inbound, SlackChannelType, SlackInboundFilter, SlackMessageEvent,
};

const BOT: &str = "U0BOT";

fn filter<'a>() -> SlackInboundFilter<'a> {
    SlackInboundFilter {
        free_response_channels: &[],
        allowed_users: &[],
        allow_all_users: true,
        thread_sessions_per_user: true,
        active_threads: &[],
        allowed_channels: &[],
        ignored_channels: &[],
        thread_require_mention: false,
        paired_users: &[],
        bot_user_id: BOT,
    }
}

fn channel_event(ts: &str, thread_ts: Option<&str>, text: &str) -> SlackMessageEvent {
    SlackMessageEvent {
        event_id: format!("Ev{ts}"),
        team_id: "T1".into(),
        channel: "C1".into(),
        channel_type: SlackChannelType::Channel,
        user: "U1".into(),
        text: text.into(),
        ts: ts.into(),
        thread_ts: thread_ts.map(str::to_string),
        bot_id: None,
        files: Vec::new(),
        mentions_bot: text.contains(&format!("<@{BOT}>")),
    }
}

#[test]
fn channel_root_anchors_its_own_thread_session() {
    let f = SlackInboundFilter {
        free_response_channels: &["C1".to_string()],
        ..filter()
    };
    let event = event_to_inbound(&channel_event("1000.000001", None, "hello"), &f)
        .expect("root should map");
    assert_eq!(event.session.platform, "slack");
    assert_eq!(event.session.guild_id.as_deref(), Some("T1"));
    assert_eq!(event.session.channel_id, "C1");
    assert_eq!(event.session.thread_id.as_deref(), Some("1000.000001"));
    assert_eq!(event.session.user_id, "U1");
    assert_eq!(event.session.bot_id.as_deref(), Some(BOT));
    assert_eq!(event.platform_message_id, "1000.000001");
    assert_eq!(event.content, "hello");
    assert_eq!(event.delivery_id.as_deref(), Some("slack:Ev1000.000001"));
}

#[test]
fn thread_reply_reuses_root_session_and_distinct_channel_is_distinct() {
    let filter = SlackInboundFilter {
        free_response_channels: &["C1".to_string(), "C2".to_string()],
        ..filter()
    };
    let root = event_to_inbound(&channel_event("1000.000001", None, "root"), &filter).unwrap();
    let reply = event_to_inbound(
        &channel_event("1000.000002", Some("1000.000001"), "reply"),
        &filter,
    )
    .unwrap();
    assert_eq!(root.session, reply.session);
    assert_eq!(
        root.session.storage_key(),
        reply.session.storage_key(),
        "thread reply must continue the root session"
    );

    let mut other_channel = channel_event("1000.000003", None, "elsewhere");
    other_channel.channel = "C2".into();
    let other = event_to_inbound(&other_channel, &filter).unwrap();
    assert_ne!(other.session, root.session);
}

#[test]
fn dm_maps_to_channel_session_without_thread_anchor() {
    let mut event = channel_event("1000.000010", None, "hi");
    event.channel = "D1".into();
    event.channel_type = SlackChannelType::Im;
    let inbound = event_to_inbound(&event, &filter()).unwrap();
    assert_eq!(inbound.session.thread_id, None);
    assert_eq!(inbound.session.channel_id, "D1");
}

#[test]
fn unmentioned_channel_message_outside_free_response_is_dropped() {
    assert!(event_to_inbound(&channel_event("1.1", None, "drive-by"), &filter()).is_none());

    let f = SlackInboundFilter {
        active_threads: &["1000.000001".to_string()],
        ..filter()
    };
    assert!(event_to_inbound(
        &channel_event("1.2", Some("1000.000001"), "follow-up"),
        &f
    )
    .is_some());

    let f = SlackInboundFilter {
        active_threads: &["1000.000001".to_string()],
        thread_require_mention: true,
        ..filter()
    };
    assert!(event_to_inbound(
        &channel_event("1.3", Some("1000.000001"), "follow-up"),
        &f
    )
    .is_none());
}

#[test]
fn free_response_channel_accepts_unmentioned_messages() {
    let filter = SlackInboundFilter {
        free_response_channels: &["C1".to_string()],
        ..filter()
    };
    assert!(event_to_inbound(&channel_event("1.1", None, "anyone"), &filter).is_some());
}

#[test]
fn mention_maps_and_strips_bot_tag() {
    let inbound = event_to_inbound(
        &channel_event("1.1", None, "<@U0BOT> summarize this"),
        &filter(),
    )
    .unwrap();
    assert_eq!(inbound.content, "summarize this");
}

#[test]
fn mention_of_other_bot_only_is_dropped() {
    let mut event = channel_event("1.1", None, "<@U9OTHER> not for you");
    event.mentions_bot = false;
    assert!(event_to_inbound(&event, &filter()).is_none());
}

#[test]
fn bot_authored_and_self_messages_are_dropped() {
    let mut from_bot = channel_event("1.1", None, "bot chatter");
    from_bot.bot_id = Some("B9".into());
    assert!(event_to_inbound(&from_bot, &filter()).is_none());

    let mut from_self = channel_event("1.2", None, "echo");
    from_self.user = BOT.into();
    assert!(event_to_inbound(&from_self, &filter()).is_none());
}

#[test]
fn authorization_rules_apply_to_users() {
    let base = channel_event("1.1", None, "<@U0BOT> hi");

    let strict = SlackInboundFilter {
        allow_all_users: false,
        ..filter()
    };
    assert!(event_to_inbound(&base, &strict).is_none());

    let allowed = SlackInboundFilter {
        allow_all_users: false,
        allowed_users: &["U1".to_string()],
        ..filter()
    };
    assert!(event_to_inbound(&base, &allowed).is_some());

    let paired = SlackInboundFilter {
        allow_all_users: false,
        paired_users: &["U1".to_string()],
        ..filter()
    };
    assert!(event_to_inbound(&base, &paired).is_some());
}

#[test]
fn channel_allow_and_ignore_lists_enforced() {
    let base = channel_event("1.1", None, "<@U0BOT> hi");

    let ignored = SlackInboundFilter {
        ignored_channels: &["C1".to_string()],
        ..filter()
    };
    assert!(event_to_inbound(&base, &ignored).is_none());

    let not_in_allowlist = SlackInboundFilter {
        allowed_channels: &["C9".to_string()],
        ..filter()
    };
    assert!(event_to_inbound(&base, &not_in_allowlist).is_none());

    let mut dm = channel_event("1.2", None, "hi");
    dm.channel = "D1".into();
    dm.channel_type = SlackChannelType::Im;
    assert!(event_to_inbound(&dm, &not_in_allowlist).is_some());
}

#[test]
fn empty_content_without_files_is_dropped() {
    assert!(event_to_inbound(&channel_event("1.1", None, "   "), &filter()).is_none());
}

#[test]
fn shared_thread_user_when_per_user_sessions_disabled() {
    let filter = SlackInboundFilter {
        thread_sessions_per_user: false,
        ..filter()
    };
    let inbound = event_to_inbound(
        &channel_event("1.2", Some("1.1"), "<@U0BOT> hi"),
        &filter,
    )
    .unwrap();
    assert_eq!(inbound.session.user_id, "shared");

    // thread_sessions_per_user=false mirrors Discord's shared-thread mode, but
    // direct messages keep their per-user identity.
    let mut dm = channel_event("1.3", None, "hi");
    dm.channel = "D1".into();
    dm.channel_type = SlackChannelType::Im;
    let inbound = event_to_inbound(&dm, &filter).unwrap();
    assert_eq!(inbound.session.user_id, "U1");
}
