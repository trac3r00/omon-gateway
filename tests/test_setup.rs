mod common;

use common::MockWebApi;
use omon_gateway::{run_setup, SetupFlags};

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn slack_flags(dir: &std::path::Path) -> SetupFlags {
    SetupFlags {
        platform: Some("slack".into()),
        bot_token: Some("xoxb-setup".into()),
        app_token: Some("xapp-setup".into()),
        model: Some("gpt-4o-mini".into()),
        api_key: Some("sk-setup".into()),
        anthropic_key: None,
        force: false,
        env_file: Some(dir.join(".env")),
    }
}

#[test]
fn render_env_contains_platform_tokens_and_model() {
    let mut input = omon_gateway::DoctorInput::from_env();
    input.platform_raw = "slack".into();
    input.slack_bot_token = Some("xoxb-r".into());
    input.slack_app_token = Some("xapp-r".into());
    input.default_model = Some("gpt-4o-mini".into());
    input.openai_api_key = Some("sk-r".into());
    let rendered = omon_gateway::render_env(&input, omon_gateway::Platform::Slack);
    assert!(rendered.contains("OMON_PLATFORM=slack"));
    assert!(rendered.contains("SLACK_BOT_TOKEN=xoxb-r"));
    assert!(rendered.contains("SLACK_APP_TOKEN=xapp-r"));
    assert!(rendered.contains("DEFAULT_MODEL=gpt-4o-mini"));
    assert!(rendered.contains("OPENAI_API_KEY=sk-r"));
    assert!(!rendered.contains("DISCORD_BOT_TOKEN"));

    let rendered = omon_gateway::render_env(&input, omon_gateway::Platform::Discord);
    assert!(rendered.contains("OMON_PLATFORM=discord"));
    assert!(rendered.contains("DISCORD_BOT_TOKEN="));
}

#[tokio::test]
async fn non_interactive_setup_writes_env_and_runs_doctor() {
    let _guard = ENV_LOCK.lock().await;
    let mock = MockWebApi::start().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("SLACK_API_BASE", &mock.base);
    std::env::set_var("DATABASE_URL", format!("sqlite://{}/setup.db", dir.path().display()));
    std::env::set_var("OMON_WORKSPACE_ROOT", dir.path().join("workspace"));

    let outcome = run_setup(slack_flags(dir.path()), |_, _| {
        panic!("non-interactive setup must not prompt")
    })
    .await
    .unwrap();

    let contents = std::fs::read_to_string(&outcome.env_path).unwrap();
    assert!(contents.contains("OMON_PLATFORM=slack"));
    assert!(contents.contains("SLACK_BOT_TOKEN=xoxb-setup"));
    assert!(contents.contains("SLACK_APP_TOKEN=xapp-setup"));
    assert!(contents.contains("DEFAULT_MODEL=gpt-4o-mini"));
    assert!(outcome.report.is_ok(), "doctor report: {:?}", outcome.report);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&outcome.env_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, ".env must be owner-only");
    }
    mock.stop().await;
}

#[tokio::test]
async fn existing_env_refuses_clobber_without_force() {
    let _guard = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "PRECIOUS=1").unwrap();

    let result = run_setup(slack_flags(dir.path()), |_, _| unreachable!()).await;
    let error = result.expect_err("clobber must fail");
    assert!(
        error.to_string().contains("--force"),
        "error must mention --force: {error}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".env")).unwrap(),
        "PRECIOUS=1"
    );

    let forced = SetupFlags {
        force: true,
        ..slack_flags(dir.path())
    };
    std::env::set_var("DATABASE_URL", format!("sqlite://{}/s2.db", dir.path().display()));
    std::env::set_var("OMON_WORKSPACE_ROOT", dir.path().join("ws2"));
    std::env::set_var("SLACK_API_BASE", "http://127.0.0.1:1");
    let outcome = run_setup(forced, |_, _| unreachable!()).await.unwrap();
    let contents = std::fs::read_to_string(&outcome.env_path).unwrap();
    assert!(contents.contains("SLACK_BOT_TOKEN=xoxb-setup"));
}

#[tokio::test]
async fn interactive_prompts_fill_missing_answers() {
    let _guard = ENV_LOCK.lock().await;
    let mock = MockWebApi::start().await;
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("SLACK_API_BASE", &mock.base);
    std::env::set_var("DATABASE_URL", format!("sqlite://{}/i.db", dir.path().display()));
    std::env::set_var("OMON_WORKSPACE_ROOT", dir.path().join("ws3"));

    let flags = SetupFlags {
        env_file: Some(dir.path().join(".env")),
        ..SetupFlags::default()
    };
    let outcome = run_setup(flags, |label, default| {
        Ok(match label {
            l if l.starts_with("Platform") => "slack".to_string(),
            l if l.starts_with("Slack bot token") => "xoxb-interactive".to_string(),
            l if l.starts_with("Slack app-level token") => "xapp-interactive".to_string(),
            l if l.starts_with("Default model") => default.unwrap_or("gpt-4o-mini").to_string(),
            l if l.starts_with("OpenAI API key") => String::new(),
            other => panic!("unexpected prompt: {other}"),
        })
    })
    .await
    .unwrap();

    let contents = std::fs::read_to_string(&outcome.env_path).unwrap();
    assert!(contents.contains("OMON_PLATFORM=slack"));
    assert!(contents.contains("SLACK_BOT_TOKEN=xoxb-interactive"));
    assert!(contents.contains("SLACK_APP_TOKEN=xapp-interactive"));
    assert!(contents.contains("DEFAULT_MODEL=gpt-4o-mini"));
    assert!(!contents.contains("OPENAI_API_KEY"), "blank key must be omitted");
    mock.stop().await;
}
