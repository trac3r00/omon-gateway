use std::path::PathBuf;

use crate::platform::Platform;
use crate::slack::SlackWebClient;

pub const DEFAULT_DISCORD_API_BASE: &str = "https://discord.com/api/v10";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl CheckStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
        }
    }
}

#[derive(Clone, Debug)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: Option<String>,
    pub hint: Option<String>,
}

impl DoctorCheck {
    fn pass(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status: CheckStatus::Pass,
            detail: Some(detail.into()),
            hint: None,
        }
    }

    fn warn(name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status: CheckStatus::Warn,
            detail: Some(detail.into()),
            hint: Some(hint.into()),
        }
    }

    fn fail(name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status: CheckStatus::Fail,
            detail: Some(detail.into()),
            hint: Some(hint.into()),
        }
    }

    fn skip(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status: CheckStatus::Skip,
            detail: Some(detail.into()),
            hint: None,
        }
    }
}

#[derive(Debug)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn is_ok(&self) -> bool {
        self.checks
            .iter()
            .all(|check| check.status != CheckStatus::Fail)
    }

    pub fn names(&self) -> Vec<&str> {
        self.checks.iter().map(|c| c.name.as_str()).collect()
    }

    pub fn check_named(&self, name: &str) -> &DoctorCheck {
        self.checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("missing check {name}"))
    }

    pub fn failure_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Fail)
            .count()
    }

    pub fn render(&self) -> String {
        let mut out = String::from("omon-gateway doctor\n");
        for check in &self.checks {
            out.push_str(&format!("[{}] {}", check.status.label(), check.name));
            if let Some(detail) = &check.detail {
                out.push_str(&format!(" — {detail}"));
            }
            out.push('\n');
            if let Some(hint) = &check.hint {
                out.push_str(&format!("       fix: {hint}\n"));
            }
        }
        out
    }
}

#[derive(Clone, Debug)]
pub struct DoctorInput {
    pub platform_raw: String,
    pub discord_tokens: Vec<String>,
    pub slack_bot_token: Option<String>,
    pub slack_app_token: Option<String>,
    pub slack_api_base: String,
    pub discord_api_base: String,
    pub default_model: Option<String>,
    pub openai_api_key: Option<String>,
    pub openai_api_base: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub anthropic_base_url: Option<String>,
    pub database_url: String,
    pub workspace_root: PathBuf,
}

impl DoctorInput {
    pub fn from_env() -> Self {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let mut discord_tokens = Vec::new();
        for name in ["DISCORD_BOT_TOKEN", "DISCORD_BOT_TOKENS"] {
            if let Some(raw) = env(name) {
                for token in raw.split(',') {
                    let token = token.trim().trim_matches('"').trim_matches('\'');
                    if !token.is_empty() && !discord_tokens.contains(&token.to_string()) {
                        discord_tokens.push(token.to_string());
                    }
                }
            }
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            platform_raw: std::env::var("OMON_PLATFORM").unwrap_or_default(),
            discord_tokens,
            slack_bot_token: env("SLACK_BOT_TOKEN"),
            slack_app_token: env("SLACK_APP_TOKEN"),
            slack_api_base: env("SLACK_API_BASE")
                .map(|base| base.trim_end_matches('/').to_string())
                .unwrap_or_else(|| crate::slack::DEFAULT_SLACK_API_BASE.to_string()),
            discord_api_base: env("DISCORD_API_BASE")
                .map(|base| base.trim_end_matches('/').to_string())
                .unwrap_or_else(|| DEFAULT_DISCORD_API_BASE.to_string()),
            default_model: env("DEFAULT_MODEL"),
            openai_api_key: env("OPENAI_API_KEY"),
            openai_api_base: env("OPENAI_API_BASE"),
            anthropic_api_key: env("ANTHROPIC_API_KEY"),
            anthropic_base_url: env("ANTHROPIC_BASE_URL"),
            database_url: env("DATABASE_URL")
                .unwrap_or_else(|| "sqlite://omon_gateway.db".to_string()),
            workspace_root: std::env::var_os("OMON_WORKSPACE_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".omon").join("workspace")),
        }
    }
}

pub async fn run_doctor(input: &DoctorInput) -> DoctorReport {
    let mut checks = Vec::new();

    let platform = match Platform::parse(&input.platform_raw) {
        Ok(platform) => {
            checks.push(DoctorCheck::pass("platform", platform.as_str()));
            Some(platform)
        }
        Err(_) => {
            checks.push(DoctorCheck::fail(
                "platform",
                format!("unknown platform {:?}", input.platform_raw),
                "set OMON_PLATFORM=discord or OMON_PLATFORM=slack",
            ));
            None
        }
    };

    match platform {
        Some(Platform::Slack) => {
            let missing: Vec<&str> = [
                ("SLACK_BOT_TOKEN", input.slack_bot_token.as_ref()),
                ("SLACK_APP_TOKEN", input.slack_app_token.as_ref()),
            ]
            .into_iter()
            .filter_map(|(name, value)| value.is_none().then_some(name))
            .collect();
            if missing.is_empty() {
                checks.push(DoctorCheck::pass("slack tokens present", "bot + app tokens set"));
            } else {
                checks.push(DoctorCheck::fail(
                    "slack tokens present",
                    format!("missing: {}", missing.join(", ")),
                    format!(
                        "set {} — create an app at api.slack.com/apps (Socket Mode + connections:write app token)",
                        missing.join(" and ")
                    ),
                ));
            }
            match &input.slack_bot_token {
                None => checks.push(DoctorCheck::skip(
                    "slack auth.test",
                    "no bot token to validate",
                )),
                Some(token) => {
                    let client = SlackWebClient::new(input.slack_api_base.clone(), token.clone());
                    match client.auth_test().await {
                        Ok(identity) => checks.push(DoctorCheck::pass(
                            "slack auth.test",
                            format!("bot user {} in team {}", identity.user_id, identity.team_id),
                        )),
                        Err(error) => {
                            let unreachable = error.to_string().contains("request failed");
                            let hint = if unreachable {
                                "check network access to the Slack API (or SLACK_API_BASE if overridden)"
                            } else {
                                "regenerate the bot token at api.slack.com/apps and reinstall the app to your workspace"
                            };
                            checks.push(DoctorCheck::fail(
                                "slack auth.test",
                                error.to_string(),
                                hint,
                            ));
                        }
                    }
                }
            }
        }
        Some(Platform::Discord) => {
            if input.discord_tokens.is_empty() {
                checks.push(DoctorCheck::fail(
                    "discord tokens present",
                    "no bot tokens configured",
                    "set DISCORD_BOT_TOKEN from discord.com/developers/applications",
                ));
                checks.push(DoctorCheck::skip(
                    "discord get current user",
                    "no bot token to validate",
                ));
            } else {
                checks.push(DoctorCheck::pass(
                    "discord tokens present",
                    format!("{} bot token(s)", input.discord_tokens.len()),
                ));
                let token = &input.discord_tokens[0];
                let url = format!("{}/users/@me", input.discord_api_base);
                let response = reqwest::Client::new()
                    .get(&url)
                    .header("Authorization", format!("Bot {token}"))
                    .send()
                    .await;
                match response {
                    Ok(resp) if resp.status().is_success() => {
                        let body: serde_json::Value =
                            resp.json().await.unwrap_or(serde_json::Value::Null);
                        let username = body
                            .get("username")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown");
                        checks.push(DoctorCheck::pass(
                            "discord get current user",
                            format!("authenticated as {username}"),
                        ));
                    }
                    Ok(resp) if resp.status().as_u16() == 401 => {
                        checks.push(DoctorCheck::fail(
                            "discord get current user",
                            "401 Unauthorized",
                            "regenerate DISCORD_BOT_TOKEN at discord.com/developers/applications (Bot -> Reset Token)",
                        ));
                    }
                    Ok(resp) => checks.push(DoctorCheck::fail(
                        "discord get current user",
                        format!("HTTP {}", resp.status()),
                        "check the Discord API status and your network",
                    )),
                    Err(error) => checks.push(DoctorCheck::fail(
                        "discord get current user",
                        error.to_string(),
                        "check network access to discord.com",
                    )),
                }
            }
        }
        None => {
            checks.push(DoctorCheck::skip(
                "platform tokens",
                "resolve the platform check first",
            ));
        }
    }

    match &input.default_model {
        None => checks.push(DoctorCheck::fail(
            "model selection",
            "DEFAULT_MODEL is not set",
            "set DEFAULT_MODEL, e.g. gpt-4o or claude-sonnet-4-5",
        )),
        Some(model) => checks.push(DoctorCheck::pass("model selection", model.clone())),
    }
    if let Some(model) = &input.default_model {
        let anthropic = model.starts_with("claude");
        let (key, key_name, base) = if anthropic {
            (
                input.anthropic_api_key.as_ref(),
                "ANTHROPIC_API_KEY",
                input.anthropic_base_url.as_ref(),
            )
        } else {
            (
                input.openai_api_key.as_ref(),
                "OPENAI_API_KEY",
                input.openai_api_base.as_ref(),
            )
        };
        if key.is_some() {
            checks.push(DoctorCheck::pass("model credentials", format!("{key_name} set")));
        } else {
            let detail = match base {
                Some(base) => format!("no {key_name}; using custom endpoint {base}"),
                None => format!("{key_name} is not set"),
            };
            checks.push(DoctorCheck::warn(
                "model credentials",
                detail,
                format!("set {key_name} unless your endpoint needs no authentication"),
            ));
        }
    }

    let db_options = input
        .database_url
        .parse::<sqlx::sqlite::SqliteConnectOptions>()
        .map(|options| options.create_if_missing(true));
    match db_options {
        Ok(options) => match sqlx::SqlitePool::connect_with(options).await {
            Ok(pool) => match sqlx::query("SELECT 1").execute(&pool).await {
                Ok(_) => {
                    checks.push(DoctorCheck::pass("database", input.database_url.clone()));
                    pool.close().await;
                }
                Err(error) => checks.push(DoctorCheck::fail(
                    "database",
                    error.to_string(),
                    "check DATABASE_URL points to a writable sqlite database",
                )),
            },
            Err(error) => checks.push(DoctorCheck::fail(
                "database",
                error.to_string(),
                "check DATABASE_URL — parent directories must exist and be writable",
            )),
        },
        Err(error) => checks.push(DoctorCheck::fail(
            "database",
            format!("unparseable DATABASE_URL: {error}"),
            "use sqlite://<path>, e.g. sqlite://omon_gateway.db",
        )),
    }

    let workspace = &input.workspace_root;
    let probe = workspace.join(".doctor-probe");
    let workspace_result = std::fs::create_dir_all(workspace)
        .and_then(|()| std::fs::write(&probe, b"ok"))
        .and_then(|()| std::fs::remove_file(&probe));
    match workspace_result {
        Ok(()) => checks.push(DoctorCheck::pass(
            "workspace",
            format!("{} writable", workspace.display()),
        )),
        Err(error) => checks.push(DoctorCheck::fail(
            "workspace",
            format!("{}: {error}", workspace.display()),
            "check OMON_WORKSPACE_ROOT permissions or choose another directory",
        )),
    }

    DoctorReport { checks }
}
