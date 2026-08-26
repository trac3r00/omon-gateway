pub mod adapter;

pub use adapter::{
    event_to_inbound, strip_bot_mention, SlackChannelType, SlackInboundFilter, SlackMessageEvent,
};

pub const DEFAULT_SLACK_API_BASE: &str = "https://slack.com/api";
