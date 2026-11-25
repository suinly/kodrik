use clap::{CommandFactory, Parser, Subcommand, error::ErrorKind};
use kodrik::{commands, config::Config};

#[derive(Parser)]
#[command(version, about = "Launch Kodrik TUI by default", long_about = None)]
struct Cli {
    /// Kodrik configuration and state directory (default ~/.kodrik)
    #[arg(long, global = true, value_name = "PATH")]
    data_dir: Option<std::path::PathBuf>,
    /// Connect to the server agent instead of starting a local agent
    #[arg(long, conflicts_with = "tui_slot")]
    remote: bool,
    /// Remote client configuration (default remote.toml in the data directory)
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

impl Cli {
    fn validate(self) -> Result<Self, clap::Error> {
        if self.command.is_some() && (self.remote || self.tui_slot.is_some()) {
            return Err(Self::command().error(
                ErrorKind::ArgumentConflict,
                "--remote and --tui-slot are only supported without a subcommand",
            ));
        }
        Ok(self)
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Pair this device with the server through your web profile
    Connect {
        /// Server base URL for first-time setup (saves remote.toml in the data directory)
        #[arg(value_name = "SERVER_URL")]
        server: Option<String>,
        #[arg(long, conflicts_with = "server")]
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse().validate().unwrap_or_else(|error| error.exit());
    if let Some(path) = &cli.data_dir {
        kodrik::config::set_data_dir(path)?;
    }
    let runtime = tokio::runtime::Runtime::new()?;
    let restart = runtime.block_on(run(cli));
    // Drop all async tasks and their sockets before replacing the process image.
    drop(runtime);
    if restart? {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let error = std::process::Command::new(std::env::current_exe()?)
                .args(std::env::args_os().skip(1))
                .exec();
            return Err(error.into());
        }
        #[cfg(windows)]
        {
            std::process::Command::new(std::env::current_exe()?)
                .args(std::env::args_os().skip(1))
                .spawn()?;
        }
    }
    Ok(())
}

async fn run(cli: Cli) -> anyhow::Result<bool> {
    if let Some(Commands::RemoteSupervisorWorker) = &cli.command {
        return commands::remote_supervisor_worker().map(|_| false);
    }
    if let Some(Commands::UiViewWorker { input, output }) = &cli.command {
        return commands::web::evaluate_views(input, output)
            .await
            .map(|_| false);
    }
    if cli.command.is_some() {
        env_logger::init();
    }
    if let Some(Commands::Invites(args)) = cli.command {
        return commands::invites::run(args).map(|_| false);
    }
    if let Some(Commands::Web {
        command: WebCommands::SetPassword,
    }) = &cli.command
    {
        return commands::web::set_password().map(|_| false);
    }
    if let Some(Commands::Connect {
        server,
        remote_config,
    }) = &cli.command
    {
        return commands::connect::run(server.as_deref(), remote_config.as_deref())
            .await
            .map(|_| false);
    }
    if cli.remote {
        return commands::connect::remote_tui(
            cli.remote_config.as_deref(),
            cli.remote_session.as_deref(),
        )
        .await
        .map(|_| false);
    }
    let config = match &cli.command {
        None | Some(Commands::Ask { .. }) => commands::setup::load_local(false)?,
        Some(Commands::Gateway) => commands::setup::load_local(true)?,
        _ => Config::load()?,
    };

    match cli.command {
        Some(Commands::Connect { .. }) => unreachable!("Pairing handled before provider config"),
        Some(Commands::RemoteSupervisorWorker) => unreachable!("Worker handled before config"),
        Some(Commands::Runs(args)) => commands::runs::run(&config, args).await?,
        Some(Commands::Invites(_)) => unreachable!("Invitations handled before provider config"),
        None => commands::tui::run(&config, cli.tui_slot).await?,
        Some(Commands::Ask { prompt }) => commands::ask::run(&config, prompt).await?,
        Some(Commands::Gateway) => return commands::gateway::run(&config).await,
        Some(Commands::RequestRestart { pid }) => commands::request_restart::run(pid)?,
        Some(Commands::Web { .. }) => {
            unreachable!("Web commands are handled before loading provider config")
        }
        Some(Commands::UiViewWorker { .. }) => {
            unreachable!("View worker handled before config loading")
        }
    }

    Ok(false)
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
            vec![
                "kodrik",
                "connect",
                "https://example.com",
                "--remote-config",
                "client.toml",
            ],
        ] {
            assert!(Cli::try_parse_from(args).and_then(Cli::validate).is_err());
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
        assert!(
            Cli::try_parse_from(["kodrik", "--tui-slot", "2", "gateway"])
                .and_then(Cli::validate)
                .is_err()
        );
    }

    #[test]
    fn data_dir_is_global_for_all_public_commands() {
        for command in [
            &[][..],
            &["--remote"],
            &["ask", "hello"],
            &["gateway"],
            &["connect"],
            &["request-restart", "42"],
            &["invites", "list"],
            &["runs", "--store", "local", "list"],
            &["web", "set-password"],
        ] {
            let flag = ["--data-dir", "custom data"];
            for args in [flag.iter().chain(command), command.iter().chain(&flag)] {
                let cli = Cli::try_parse_from(std::iter::once(&"kodrik").chain(args))
                    .and_then(Cli::validate)
                    .unwrap();
                assert_eq!(cli.data_dir.unwrap(), std::path::Path::new("custom data"));
            }
        }
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
