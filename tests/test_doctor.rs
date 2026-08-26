mod common;

use common::MockWebApi;
use omon_gateway::{CheckStatus, DoctorInput, DEFAULT_SLACK_API_BASE};

fn base_input() -> DoctorInput {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    DoctorInput {
        platform_raw: String::new(),
        discord_tokens: Vec::new(),
        slack_bot_token: None,
        slack_app_token: None,
        slack_api_base: DEFAULT_SLACK_API_BASE.to_string(),
        discord_api_base: "https://discord.com/api/v10".into(),
        default_model: Some("gpt-4o-mini".into()),
        openai_api_key: Some("sk-test".into()),
        openai_api_base: None,
        anthropic_api_key: None,
        anthropic_base_url: None,
        database_url: format!("sqlite://{}/doctor.db", path.display()),
        workspace_root: path.join("workspace"),
    }
}

fn status_of(report: &omon_gateway::DoctorReport, name: &str) -> CheckStatus {
    report
        .checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("missing check {name}; got {:?}", report.names()))
        .status
}

#[tokio::test]
async fn slack_happy_path_all_pass() {
    let mock = MockWebApi::start().await;
    let input = DoctorInput {
        platform_raw: "slack".into(),
        slack_bot_token: Some("xoxb-test".into()),
        slack_app_token: Some("xapp-test".into()),
        slack_api_base: mock.base.clone(),
        ..base_input()
    };
    let report = omon_gateway::run_doctor(&input).await;
    assert!(report.is_ok(), "report: {report:?}");
    assert_eq!(status_of(&report, "platform"), CheckStatus::Pass);
    assert_eq!(status_of(&report, "slack tokens present"), CheckStatus::Pass);
    assert_eq!(status_of(&report, "slack auth.test"), CheckStatus::Pass);
    assert_eq!(status_of(&report, "model credentials"), CheckStatus::Pass);
    assert_eq!(status_of(&report, "database"), CheckStatus::Pass);
    assert_eq!(status_of(&report, "workspace"), CheckStatus::Pass);
    assert_eq!(
        mock.auth_for("auth.test").await.as_deref(),
        Some("Bearer xoxb-test")
    );
    mock.stop().await;
}

#[tokio::test]
async fn missing_slack_tokens_fail_with_hints_and_skip_live_check() {
    let input = DoctorInput {
        platform_raw: "slack".into(),
        ..base_input()
    };
    let report = omon_gateway::run_doctor(&input).await;
    assert!(!report.is_ok());
    let presence = report.check_named("slack tokens present");
    assert_eq!(presence.status, CheckStatus::Fail);
    assert!(
        presence.hint.as_deref().unwrap_or("").contains("SLACK_BOT_TOKEN"),
        "hint must name the missing var: {:?}",
        presence.hint
    );
    assert_eq!(status_of(&report, "slack auth.test"), CheckStatus::Skip);
}

#[tokio::test]
async fn invalid_slack_token_surfaces_api_error() {
    let mock = MockWebApi::start_with_error(Some("auth.test")).await;
    let input = DoctorInput {
        platform_raw: "slack".into(),
        slack_bot_token: Some("xoxb-bad".into()),
        slack_app_token: Some("xapp-test".into()),
        slack_api_base: mock.base.clone(),
        ..base_input()
    };
    let report = omon_gateway::run_doctor(&input).await;
    assert!(!report.is_ok());
    let live = report.check_named("slack auth.test");
    assert_eq!(live.status, CheckStatus::Fail);
    assert!(
        live.detail.as_deref().unwrap_or("").contains("channel_not_found"),
        "detail must carry the slack error code: {:?}",
        live.detail
    );
    mock.stop().await;
}

#[tokio::test]
async fn bogus_platform_fails_naming_valid_options() {
    let input = DoctorInput {
        platform_raw: "bogus".into(),
        ..base_input()
    };
    let report = omon_gateway::run_doctor(&input).await;
    assert!(!report.is_ok());
    let platform = report.check_named("platform");
    assert_eq!(platform.status, CheckStatus::Fail);
    let hint = platform.hint.as_deref().unwrap_or("");
    assert!(hint.contains("discord") && hint.contains("slack"));
}

#[tokio::test]
async fn discord_happy_path_uses_users_me() {
    let mock = MockWebApi::start().await;
    let input = DoctorInput {
        platform_raw: "discord".into(),
        discord_tokens: vec!["discord-token".into()],
        discord_api_base: mock.base.clone(),
        ..base_input()
    };
    let report = omon_gateway::run_doctor(&input).await;
    assert!(report.is_ok(), "report: {report:?}");
    assert_eq!(status_of(&report, "discord tokens present"), CheckStatus::Pass);
    assert_eq!(status_of(&report, "discord get current user"), CheckStatus::Pass);
    assert_eq!(
        mock.auth_for("users/@me").await.as_deref(),
        Some("Bot discord-token")
    );
    mock.stop().await;
}

#[tokio::test]
async fn discord_unauthorized_token_fails_live_check() {
    let mock = MockWebApi::start().await;
    *mock.recorder.fail_get_me.lock().await = true;
    let input = DoctorInput {
        platform_raw: "discord".into(),
        discord_tokens: vec!["bad-token".into()],
        discord_api_base: mock.base.clone(),
        ..base_input()
    };
    let report = omon_gateway::run_doctor(&input).await;
    assert!(!report.is_ok());
    assert_eq!(status_of(&report, "discord get current user"), CheckStatus::Fail);
    mock.stop().await;
}

#[tokio::test]
async fn missing_model_fails_and_missing_key_warns() {
    let no_model = DoctorInput {
        default_model: None,
        ..base_input()
    };
    let report = omon_gateway::run_doctor(&no_model).await;
    assert_eq!(status_of(&report, "model selection"), CheckStatus::Fail);

    let no_key = DoctorInput {
        openai_api_key: None,
        ..base_input()
    };
    let report = omon_gateway::run_doctor(&no_key).await;
    assert_eq!(status_of(&report, "model credentials"), CheckStatus::Warn);
}

#[tokio::test]
async fn unwritable_database_and_workspace_fail_with_hints() {
    let blocker = tempfile::NamedTempFile::new().unwrap();
    let blocked = blocker.path().join("child.db");
    let input = DoctorInput {
        database_url: format!("sqlite://{}", blocked.display()),
        workspace_root: blocker.path().join("workspace"),
        ..base_input()
    };
    let report = omon_gateway::run_doctor(&input).await;
    assert!(!report.is_ok());
    assert_eq!(status_of(&report, "database"), CheckStatus::Fail);
    assert_eq!(status_of(&report, "workspace"), CheckStatus::Fail);
    assert!(
        report
            .check_named("database")
            .hint
            .as_deref()
            .unwrap_or("")
            .contains("DATABASE_URL")
    );
}
