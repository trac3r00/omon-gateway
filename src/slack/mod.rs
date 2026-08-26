pub mod adapter;
pub mod api;
pub mod egress;
pub mod pairing;
pub mod runtime;
pub mod socket;

pub use adapter::{
    event_to_inbound, strip_bot_mention, SlackChannelType, SlackInboundFilter, SlackMessageEvent,
};
pub use api::{SlackAuthIdentity, SlackHistoryMessage, SlackWebClient};
pub use egress::{approval_blocks, slack_emoji_name, SlackEgress, SLACK_MESSAGE_LIMIT};
pub use pairing::{SlackPairingOutcome, SlackPairingStore};
pub use runtime::{OwnedSlackFilter, SlackRuntime, SlackRuntimeConfig};
pub use socket::{ack_frame, SocketEvent, SocketModeClient};

pub const DEFAULT_SLACK_API_BASE: &str = "https://slack.com/api";
