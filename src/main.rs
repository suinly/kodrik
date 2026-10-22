use clap::{Parser, Subcommand};
use kodrik::{commands, config::Config};

#[derive(Parser)]
#[command(version, about = "Launch Kodrik TUI by default", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    #[command(hide = true)]
    UiViewWorker {
        input: std::path::PathBuf,
        output: std::path::PathBuf,
    },
    /// Inspect, retry or cancel durable runs in an explicit store
    Runs(commands::runs::Args),
    /// Manually manage access invitations as the local service owner
    Invites(commands::invites::Args),
    /// CLI mode
    Ask { prompt: String },
    /// Run gateway mode
    Gateway,
    /// Request a cooperative restart from a running gateway
    RequestRestart { pid: u32 },
    /// Manage the web page server
    Web {
        #[command(subcommand)]
        command: WebCommands,
    },
}

#[derive(Subcommand)]
enum WebCommands {
    /// Set the owner password and revoke all existing web sessions
    SetPassword,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Some(Commands::UiViewWorker { input, output }) = &cli.command {
        return commands::web::evaluate_views(input, output).await;
    }
    if cli.command.is_some() {
        env_logger::init();
    }
    if let Some(Commands::Invites(args)) = cli.command {
        return commands::invites::run(args);
    }
    if let Some(Commands::Web {
        command: WebCommands::SetPassword,
    }) = &cli.command
    {
        return commands::web::set_password();
    }
    let config = Config::load()?;

    match cli.command {
        Some(Commands::Runs(args)) => commands::runs::run(&config, args).await?,
        Some(Commands::Invites(_)) => unreachable!("Invitations handled before provider config"),
        None => commands::tui::run(&config).await?,
        Some(Commands::Ask { prompt }) => commands::ask::run(&config, prompt).await?,
        Some(Commands::Gateway) => commands::gateway::run(&config).await?,
        Some(Commands::RequestRestart { pid }) => commands::request_restart::run(pid)?,
        Some(Commands::Web { .. }) => {
            unreachable!("Web commands are handled before loading provider config")
        }
        Some(Commands::UiViewWorker { .. }) => {
            unreachable!("View worker handled before config loading")
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_runs_with_required_store() {
        for action in ["show", "retry", "cancel"] {
            assert!(
                Cli::try_parse_from(["kodrik", "runs", "--store", "web", action, "run-id"]).is_ok()
            );
        }
        assert!(Cli::try_parse_from(["kodrik", "runs", "--store", "local", "list"]).is_ok());
        assert!(Cli::try_parse_from(["kodrik", "runs", "list"]).is_err());
        assert!(Cli::try_parse_from(["kodrik", "runs", "--store", "../other", "list"]).is_err());
    }
    #[test]
    fn parses_web_password_command_without_provider_config() {
        assert!(Cli::try_parse_from(["kodrik", "web", "set-password"]).is_ok());
        assert!(Cli::try_parse_from(["kodrik", "web", "set-password", "secret"]).is_err());
    }

    #[test]
    fn invites_are_manual_and_expiry_is_bounded() {
        assert!(Cli::try_parse_from(["kodrik", "invites", "create", "--label", "Alice"]).is_ok());
        assert!(Cli::try_parse_from(["kodrik", "invites", "create"]).is_err());
        for hours in ["0", "721"] {
            assert!(
                Cli::try_parse_from([
                    "kodrik",
                    "invites",
                    "create",
                    "--label",
                    "Alice",
                    "--expires-in-hours",
                    hours
                ])
                .is_err()
            );
        }
        assert!(Cli::try_parse_from(["kodrik", "invites", "list"]).is_ok());
        assert!(Cli::try_parse_from(["kodrik", "invites", "revoke-user", "7"]).is_ok());
    }
}
