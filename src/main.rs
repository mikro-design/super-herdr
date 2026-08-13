use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use super_herdr::clipboard;
use super_herdr::config::{Config, Target};
use super_herdr::probe::{FederationReport, probe_all};
use super_herdr::transport::expand_discovered_sessions;
use super_herdr::tui;

#[derive(Debug, Parser)]
#[command(
    name = "super-herdr",
    version,
    about = "Federate Herdr sessions across several machines"
)]
struct Cli {
    /// Configuration file. Falls back to SUPER_HERDR_CONFIG or the XDG path.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Parse and validate the federation configuration.
    Check,
    /// Query all configured Herdr sessions concurrently.
    Probe {
        /// Emit one machine-readable federation report.
        #[arg(long)]
        json: bool,
        /// Include full server snapshots in JSON output.
        #[arg(long, requires = "json")]
        snapshots: bool,
        /// Override the configured command timeout.
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,
    },
    /// Open the federated terminal UI.
    Tui,
    /// Inspect clipboard copy and paste capabilities without reading clipboard contents.
    Clipboard {
        #[command(subcommand)]
        command: ClipboardCommands,
    },
    /// Add or inspect configured Herdr hosts.
    Target {
        #[command(subcommand)]
        command: TargetCommands,
    },
}

#[derive(Debug, Subcommand)]
enum ClipboardCommands {
    /// Report the active native or terminal-mediated clipboard paths.
    Check,
}

#[derive(Debug, Subcommand)]
enum TargetCommands {
    /// Add a local or SSH Herdr host to the TOML configuration.
    Add {
        /// Stable name used to qualify this host's Herdr IDs.
        name: String,
        /// OpenSSH destination or alias.
        #[arg(
            long,
            value_name = "DESTINATION",
            required_unless_present = "local",
            conflicts_with = "local"
        )]
        ssh: Option<String>,
        /// Run Herdr directly on this desktop instead of over SSH.
        #[arg(long, conflicts_with = "ssh")]
        local: bool,
        /// Discover all running Herdr sessions when Super-Herdr starts.
        #[arg(long)]
        discover_sessions: bool,
        /// Use one named session, or retain it as the discovery fallback.
        #[arg(long, value_name = "NAME")]
        session: Option<String>,
        /// Absolute Herdr API socket path on the target.
        #[arg(long, value_name = "PATH")]
        socket: Option<String>,
        /// Herdr client candidate on the target; repeat for fallbacks.
        #[arg(long = "herdr-bin", value_name = "COMMAND")]
        herdr_bins: Vec<String>,
    },
    /// List configured Herdr hosts without connecting to them.
    List,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Commands::Tui);
    if matches!(
        &command,
        Commands::Clipboard {
            command: ClipboardCommands::Check
        }
    ) {
        for line in clipboard::diagnostic_lines() {
            println!("{line}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let command = match command {
        Commands::Target { command } => {
            return run_target_command(cli.config.as_deref(), command);
        }
        command => command,
    };
    let (config, path) = Config::load(cli.config.as_deref())?;

    match command {
        Commands::Check => {
            println!(
                "{}: {} valid target(s)",
                path.display(),
                config.targets.len()
            );
            for target in &config.targets {
                let session_scope = if target.discover_sessions {
                    format!("discover sessions (fallback {})", target.session_name())
                } else {
                    format!("session {}", target.session_name())
                };
                println!(
                    "  {}: {} / {} / {}",
                    target.name,
                    target.endpoint(),
                    session_scope,
                    target.candidate_bins().collect::<Vec<_>>().join(", ")
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Probe {
            json,
            snapshots,
            timeout,
        } => {
            let config = expand_discovered_sessions(config).await;
            let timeout =
                Duration::from_secs(timeout.unwrap_or(config.transport.command_timeout_seconds));
            let mut reports = probe_all(&config, timeout).await?;
            let failed = reports.iter().filter(|report| !report.ok).count();

            if json {
                if !snapshots {
                    reports
                        .iter_mut()
                        .for_each(|report| report.discard_snapshot());
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&FederationReport {
                        config: path.display().to_string(),
                        targets: reports,
                    })?
                );
            } else {
                for report in &reports {
                    if report.ok {
                        println!(
                            "OK   {}/{} [{}] Herdr {} protocol {} via {}: {} workspace(s), {} pane(s), {} agent(s) ({} ms)",
                            report.target,
                            report.session,
                            report.endpoint,
                            report.herdr_version.as_deref().unwrap_or("unknown"),
                            report
                                .protocol
                                .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                            report.herdr_bin.as_deref().unwrap_or("unknown"),
                            report.workspaces,
                            report.panes,
                            report.agents,
                            report.elapsed_ms
                        );
                    } else {
                        println!(
                            "FAIL {}/{} [{}]: {} ({} ms)",
                            report.target,
                            report.session,
                            report.endpoint,
                            report.error.as_deref().unwrap_or("unknown error"),
                            report.elapsed_ms
                        );
                    }
                }
            }

            Ok(if failed == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Commands::Tui => {
            tui::run(expand_discovered_sessions(config).await).await?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Clipboard { .. } => unreachable!("clipboard commands return before config load"),
        Commands::Target { .. } => unreachable!("target commands return before config load"),
    }
}

fn run_target_command(
    config_path: Option<&std::path::Path>,
    command: TargetCommands,
) -> Result<ExitCode> {
    match command {
        TargetCommands::Add {
            name,
            ssh,
            local: _,
            discover_sessions,
            session,
            socket,
            herdr_bins,
        } => {
            let target = Target {
                name: name.clone(),
                ssh,
                discover_sessions,
                session,
                socket,
                herdr_bins: if herdr_bins.is_empty() {
                    vec!["herdr".to_owned()]
                } else {
                    herdr_bins
                },
            };
            let path = Config::add_target_file(config_path, target)?;
            println!("added target {name:?} to {}", path.display());
            println!("run `super-herdr probe` to verify it");
            Ok(ExitCode::SUCCESS)
        }
        TargetCommands::List => {
            let (config, path) = Config::load(config_path)?;
            println!(
                "{}: {} configured target(s)",
                path.display(),
                config.targets.len()
            );
            for target in &config.targets {
                let scope = if target.discover_sessions {
                    "all running sessions"
                } else {
                    target.session_name()
                };
                println!("  {}: {} / {scope}", target.name, target.endpoint());
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}
