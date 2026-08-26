use std::fmt;
use std::str::FromStr;

use crate::error::OmonError;

/// Chat platform the gateway serves. Discord remains the default; Slack is an
/// opt-in alternative selected via `OMON_PLATFORM` or `--platform`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Platform {
    Discord,
    Slack,
}

impl Platform {
    /// Parses a platform name. Empty input selects the default (Discord) so
    /// unset optional configuration keeps existing behavior.
    pub fn parse(raw: &str) -> crate::Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Self::Discord);
        }
        Self::from_str(trimmed)
    }

    pub fn from_env() -> crate::Result<Self> {
        Self::parse(&std::env::var("OMON_PLATFORM").unwrap_or_default())
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Discord => "discord",
            Self::Slack => "slack",
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Platform {
    type Err = OmonError;

    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "discord" => Ok(Self::Discord),
            "slack" => Ok(Self::Slack),
            other => Err(OmonError::Config(format!(
                "unknown platform \"{other}\": valid options are discord|slack"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Platform;

    #[test]
    fn from_str_rejects_empty_but_parse_defaults() {
        assert!("".parse::<Platform>().is_err());
        assert_eq!(Platform::parse("").unwrap(), Platform::Discord);
    }
}
