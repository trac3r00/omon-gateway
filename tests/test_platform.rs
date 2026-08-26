use omon_gateway::Platform;

#[test]
fn parses_discord_and_slack_case_insensitively() {
    assert_eq!(Platform::parse("discord").unwrap(), Platform::Discord);
    assert_eq!(Platform::parse("DISCORD").unwrap(), Platform::Discord);
    assert_eq!(Platform::parse(" slack ").unwrap(), Platform::Slack);
    assert_eq!(Platform::parse("SLACK").unwrap(), Platform::Slack);
}

#[test]
fn rejects_unknown_platform_naming_valid_options() {
    let error = Platform::parse("bogus").unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("discord") && message.contains("slack"),
        "error must name valid options, got: {message}"
    );
    assert!(
        message.contains("bogus"),
        "error must echo the offending value, got: {message}"
    );
}

#[test]
fn defaults_to_discord_when_unset() {
    assert_eq!(Platform::parse("").unwrap(), Platform::Discord);
}

#[test]
fn display_round_trips_through_parse() {
    for platform in [Platform::Discord, Platform::Slack] {
        assert_eq!(Platform::parse(&platform.to_string()).unwrap(), platform);
    }
}
