use clap::{Parser, Subcommand};
use omon_gateway::migrate::MigrateArgs;
use omon_gateway::{OmonError, Result};
use tokio_util::sync::CancellationToken;

#[allow(dead_code, private_interfaces)]
mod legacy {
    include!("main.rs");

    pub async fn run_gateway_public() -> Result<()> {
        run_gateway().await
    }

    #[allow(unused_imports)]
    pub mod dashboard {
        use tracing_subscriber::util::SubscriberInitExt;

        trait OptionStringTrim {
            fn trim(&self) -> &str;
        }

        impl OptionStringTrim for Option<String> {
            fn trim(&self) -> &str {
                self.as_deref().unwrap_or_default().trim()
            }
        }

        include!("dashboard.rs");
    }

    pub mod dashboard_runtime {
        include!("dashboard_runtime.rs");
    }
}

#[derive(Debug, Parser)]
#[command(name = "omon-gateway", version, about = "Ultra-fast AI agent gateway for Discord and Slack")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Default, clap::Args)]
pub struct RunArgs {
    /// Chat platform to serve: discord (default) or slack. Overrides OMON_PLATFORM.
    #[arg(long, value_parser = clap::value_parser!(omon_gateway::Platform))]
    platform: Option<omon_gateway::Platform>,
}

#[derive(Debug, Default, clap::Args)]
pub struct SetupCliArgs {
    /// Chat platform: discord or slack.
    #[arg(long)]
    platform: Option<String>,
    /// Bot token (DISCORD_BOT_TOKEN or SLACK_BOT_TOKEN).
    #[arg(long)]
    bot_token: Option<String>,
    /// Slack app-level token (xapp-...; slack only).
    #[arg(long)]
    app_token: Option<String>,
    /// Default model, e.g. gpt-4o-mini or claude-sonnet-4-5.
    #[arg(long)]
    model: Option<String>,
    /// OpenAI-compatible API key.
    #[arg(long)]
    api_key: Option<String>,
    /// Anthropic API key (for claude-* models).
    #[arg(long)]
    anthropic_key: Option<String>,
    /// Overwrite an existing .env file.
    #[arg(long)]
    force: bool,
    /// Target file (default ./.env).
    #[arg(long)]
    env_file: Option<std::path::PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the Discord gateway and, when enabled by environment, the dashboard.
    Run(RunArgs),
    /// Preflight-check the environment: platform, tokens, model, database, workspace.
    Doctor,
    /// Guided setup: generate a validated .env for Discord or Slack.
    Setup(SetupCliArgs),
    /// Run only the Web Dashboard HTTP/WebSocket server.
    Dashboard(legacy::dashboard::DashboardArgs),
    /// Alias for `dashboard`.
    Serve(legacy::dashboard::DashboardArgs),
    /// Run database migration utilities.
    Migrate(MigrateArgs),
}

impl Cli {
    fn into_command(self) -> Command {
        self.command.unwrap_or(Command::Run(RunArgs::default()))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    if let Some(home) = std::env::var_os("HOME") {
        let fallback = std::path::PathBuf::from(home).join(".omon").join(".env");
        let _ = dotenvy::from_path(fallback);
    }
    legacy::dashboard::init_tracing();

    match Cli::parse().into_command() {
        Command::Run(args) => {
            if let Some(platform) = args.platform {
                std::env::set_var("OMON_PLATFORM", platform.as_str());
            }
            run_gateway_with_optional_dashboard().await
        }
        Command::Dashboard(args) | Command::Serve(args) => {
            legacy::dashboard_runtime::run_standalone_cli(
                legacy::dashboard::DashboardSettings::from_args(args),
            )
            .await
        }
        Command::Migrate(args) => omon_gateway::migrate::run_migrate(args).await,
        Command::Doctor => run_doctor_cli().await,
        Command::Setup(args) => run_setup_cli(args).await,
    }
}

async fn run_doctor_cli() -> Result<()> {
    let report = omon_gateway::run_doctor(&omon_gateway::DoctorInput::from_env()).await;
    print!("{}", report.render());
    if report.is_ok() {
        Ok(())
    } else {
        Err(OmonError::Config(format!(
            "doctor: {} check(s) failed",
            report.failure_count()
        )))
    }
}

async fn run_setup_cli(args: SetupCliArgs) -> Result<()> {
    use std::io::IsTerminal;

    let interactive = std::io::stdin().is_terminal();
    let prompt = move |label: &str, default: Option<&str>| -> Result<String> {
        if !interactive {
            return Err(OmonError::Config(format!(
                "non-interactive terminal and no answer for \"{label}\" — pass the matching --flag (see setup --help)"
            )));
        }
        match default {
            Some(default) => eprint!("{label} [{default}]: "),
            None => eprint!("{label}: "),
        }
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|error| OmonError::Config(format!("failed to read input: {error}")))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            Ok(default.unwrap_or_default().to_string())
        } else {
            Ok(trimmed.to_string())
        }
    };

    let flags = omon_gateway::SetupFlags {
        platform: args.platform,
        bot_token: args.bot_token,
        app_token: args.app_token,
        model: args.model,
        api_key: args.api_key,
        anthropic_key: args.anthropic_key,
        force: args.force,
        env_file: args.env_file,
    };
    let outcome = omon_gateway::run_setup(flags, prompt).await?;
    println!("wrote {}", outcome.env_path.display());
    print!("{}", outcome.report.render());
    if outcome.report.is_ok() {
        println!("setup complete — start the gateway with `omon-gateway run`");
        Ok(())
    } else {
        Err(OmonError::Config(format!(
            "setup: doctor reported {} failing check(s) — see hints above",
            outcome.report.failure_count()
        )))
    }
}

async fn run_gateway_with_optional_dashboard() -> Result<()> {
    let settings = legacy::dashboard::DashboardSettings::from_env();
    if !settings.enabled {
        return legacy::run_gateway_public().await;
    }
    settings.validate()?;

    let shutdown = CancellationToken::new();
    let dashboard_shutdown = shutdown.clone();
    let dashboard = legacy::dashboard_runtime::run_standalone(settings, dashboard_shutdown, false);
    let gateway = legacy::run_gateway_public();
    tokio::pin!(dashboard);
    tokio::pin!(gateway);

    tokio::select! {
        result = &mut gateway => {
            shutdown.cancel();
            dashboard.await?;
            result
        }
        result = &mut dashboard => {
            match result {
                Ok(()) => Err(OmonError::Config("dashboard server stopped unexpectedly while the Discord gateway was running".into())),
                Err(error) => Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command};

    #[test]
    fn cli_defaults_to_gateway_run() {
        assert!(matches!(
            Cli::try_parse_from(["omon-gateway"])
                .unwrap()
                .into_command(),
            Command::Run(_)
        ));
    }

    #[test]
    fn cli_run_accepts_platform_flag() {
        let cli = Cli::try_parse_from(["omon-gateway", "run", "--platform", "slack"]).unwrap();
        match cli.into_command() {
            Command::Run(args) => {
                assert_eq!(args.platform, Some(omon_gateway::Platform::Slack))
            }
            other => panic!("expected run command, got {other:?}"),
        }

        let default = Cli::try_parse_from(["omon-gateway", "run"]).unwrap();
        match default.into_command() {
            Command::Run(args) => assert_eq!(args.platform, None),
            other => panic!("expected run command, got {other:?}"),
        }

        assert!(Cli::try_parse_from(["omon-gateway", "run", "--platform", "bogus"]).is_err());
    }

    #[test]
    fn cli_accepts_dashboard_and_serve_alias() {
        let dashboard = Cli::try_parse_from([
            "omon-gateway",
            "dashboard",
            "--host",
            "127.0.0.1",
            "--port",
            "9120",
        ])
        .unwrap();
        match dashboard.into_command() {
            Command::Dashboard(args) => {
                assert_eq!(args.host, "127.0.0.1");
                assert_eq!(args.port, 9120);
            }
            other => panic!("expected dashboard command, got {other:?}"),
        }

        assert!(matches!(
            Cli::try_parse_from(["omon-gateway", "serve", "--insecure"])
                .unwrap()
                .into_command(),
            Command::Serve(_)
        ));
    }
}
