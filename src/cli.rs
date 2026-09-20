use crate::config::AppConfig;
use crate::proxy::serve;
use crate::service_control::{restart_service, start_service, status_service, stop_service};
use crate::token::{
    login_with_device_flow, printable_token_from_auth_config,
    should_try_device_login_after_auth_error,
};
use clap::{Parser, Subcommand};
use std::io::{self, Write};
use tracing_subscriber::EnvFilter;

const DEFAULT_LOG_FILTER: &str = "codex_code_router=info,warn";

#[derive(Debug, Parser)]
#[command(about = "Local Codex -> GitHub Copilot Responses API adapter")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the local HTTP adapter service in the foreground.
    Serve,
    /// Start the local adapter service in the background.
    Start,
    /// Stop the background adapter service.
    Stop,
    /// Restart the background adapter service.
    Restart,
    /// Show whether the local adapter service is reachable.
    Status,
    /// Run interactive GitHub Copilot device login and save the token file.
    Login,
    /// Print the configured Copilot bearer token only.
    PrintToken,
}

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            init_tracing();
            let config = AppConfig::from_env();
            serve(config).await
        }
        Command::Start => {
            let config = AppConfig::from_env();
            ensure_auth_ready_or_login(&config).await?;
            start_service(&config)
        }
        Command::Stop => stop_service(&AppConfig::from_env()),
        Command::Restart => {
            let config = AppConfig::from_env();
            ensure_auth_ready_or_login(&config).await?;
            restart_service(&config)
        }
        Command::Status => status_service(&AppConfig::from_env()),
        Command::Login => login().await,
        Command::PrintToken => print_token().await,
    }
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .with_ansi(false)
        .init();
}

async fn print_token() -> anyhow::Result<()> {
    let config = AppConfig::from_env();
    let client = reqwest::Client::builder()
        .timeout(config.request_timeout)
        .build()?;
    match printable_token_from_auth_config(&config.auth, &config.headers, &client).await {
        Ok(token) => {
            io::stdout().write_all(token.as_bytes())?;
            Ok(())
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

async fn login() -> anyhow::Result<()> {
    let config = AppConfig::from_env();
    let client = reqwest::Client::builder()
        .timeout(config.request_timeout)
        .build()?;
    let mut stderr = io::stderr();
    login_with_device_flow(&config.auth, &config.headers, &client, &mut stderr).await?;
    Ok(())
}

async fn ensure_auth_ready_or_login(config: &AppConfig) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(config.request_timeout)
        .build()?;

    match printable_token_from_auth_config(&config.auth, &config.headers, &client).await {
        Ok(_) => Ok(()),
        Err(error) if should_try_device_login_after_auth_error(&error) => {
            eprintln!("Copilot auth is unavailable: {error}");
            let mut stderr = io::stderr();
            login_with_device_flow(&config.auth, &config.headers, &client, &mut stderr).await?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}
