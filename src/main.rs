use clap::{Parser, Subcommand};
use kodrik::{commands, config::Config};

#[derive(Parser)]
#[command(version, about = "Launch Kodrik TUI by default", long_about = None, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Connect to the server agent instead of starting a local agent
    #[arg(long, conflicts_with = "tui_slot")]
    remote: bool,
    /// Remote client configuration (default ~/.kodrik/remote.toml)
    #[arg(long, requires = "remote")]
    remote_config: Option<std::path::PathBuf>,
    /// Resume a server conversation in remote TUI
    #[arg(long, requires = "remote")]
    remote_session: Option<String>,
    /// Restore a specific TUI conversation slot
    #[arg(long)]
    tui_slot: Option<std::num::NonZeroU64>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Pair this device with the server through the owner's web profile
    Connect {
        #[arg(long)]
        remote_config: Option<std::path::PathBuf>,
    },
    #[command(hide = true)]
    RemoteSupervisorWorker,
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
    if let Some(Commands::RemoteSupervisorWorker) = &cli.command {
        return commands::remote_supervisor_worker();
    }
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
    if let Some(Commands::Connect { remote_config }) = &cli.command {
        return commands::connect::run(remote_config.as_deref()).await;
    }
    if cli.remote {
        return commands::connect::remote_tui(
            cli.remote_config.as_deref(),
            cli.remote_session.as_deref(),
        )
        .await;
    }
    let config = Config::load()?;

    match cli.command {
        Some(Commands::Connect { .. }) => unreachable!("Pairing handled before provider config"),
        Some(Commands::RemoteSupervisorWorker) => unreachable!("Worker handled before config"),
        Some(Commands::Runs(args)) => commands::runs::run(&config, args).await?,
        Some(Commands::Invites(_)) => unreachable!("Invitations handled before provider config"),
        None => commands::tui::run(&config, cli.tui_slot).await?,
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
    fn remote_cli_is_explicit_and_conflicts_with_standalone_commands() {
        assert!(
            Cli::try_parse_from(["kodrik", "--remote", "--remote-config", "client.toml"]).is_ok()
        );
        assert!(
            Cli::try_parse_from(["kodrik", "connect", "--remote-config", "client.toml"]).is_ok()
        );
        for args in [
            vec!["kodrik", "--remote", "gateway"],
            vec!["kodrik", "--remote", "ask", "hello"],
            vec!["kodrik", "--remote", "--tui-slot", "2"],
            vec!["kodrik", "--remote-config", "client.toml"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn remote_session_requires_remote_mode_and_preserves_custom_config() {
        let cli = Cli::try_parse_from([
            "kodrik",
            "--remote",
            "--remote-session",
            "server-session",
            "--remote-config",
            "client config.toml",
        ])
        .unwrap();
        assert_eq!(cli.remote_session.as_deref(), Some("server-session"));
        assert_eq!(
            cli.remote_config.as_deref(),
            Some(std::path::Path::new("client config.toml"))
        );
        assert!(Cli::try_parse_from(["kodrik", "--remote-session", "server-session"]).is_err());
    }

    #[test]
    fn tui_slot_is_positive_and_only_used_without_a_subcommand() {
        assert!(Cli::try_parse_from(["kodrik", "--tui-slot", "2"]).is_ok());
        for slot in ["0", "-1", "not-a-slot"] {
            assert!(Cli::try_parse_from(["kodrik", "--tui-slot", slot]).is_err());
        }
        assert!(Cli::try_parse_from(["kodrik", "--tui-slot", "2", "gateway"]).is_err());
    }

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
