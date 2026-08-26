pub mod adapter;
pub mod api;

pub use adapter::{
    event_to_inbound, strip_bot_mention, SlackChannelType, SlackInboundFilter, SlackMessageEvent,
};
pub use api::{SlackAuthIdentity, SlackHistoryMessage, SlackWebClient};

pub const DEFAULT_SLACK_API_BASE: &str = "https://slack.com/api";
