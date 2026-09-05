use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use super_herdr::clipboard;
use super_herdr::config::{Config, Target};
use super_herdr::daemon::{DaemonOptions, serve};
use super_herdr::notifications;
use super_herdr::probe::{FederationReport, probe_all};
use super_herdr::transport::expand_discovered_sessions;
use super_herdr::tui;

const PROBE_OK_PLAIN: &str = "OK   ";
const PROBE_OK_COLOR: &str = "\x1b[32mOK\x1b[0m   ";
const PROBE_FAIL_PLAIN: &str = "FAIL ";
const PROBE_FAIL_COLOR: &str = "\x1b[31mFAIL\x1b[0m ";

fn probe_status_prefix(ok: bool, color: bool) -> &'static str {
    match (ok, color) {
        (true, true) => PROBE_OK_COLOR,
        (true, false) => PROBE_OK_PLAIN,
        (false, true) => PROBE_FAIL_COLOR,
        (false, false) => PROBE_FAIL_PLAIN,
    }
}

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
    /// Serve the federation on a local socket for Super-Herdr clients.
    Daemon {
        /// Socket path. Defaults to the XDG runtime directory.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
        /// Also serve the browser client on this port.
        #[arg(long, value_name = "PORT", num_args = 0..=1, default_missing_value = "8790")]
        web: Option<u16>,
        /// Address for an explicit direct browser route. This bypasses the
        /// default outbound bridge. A public address is refused because a
        /// device token authenticates but does not encrypt.
        #[arg(long, value_name = "ADDRESS", requires = "web")]
        web_address: Option<std::net::IpAddr>,
        /// Where a device reaches an operator-managed browser proxy. This
        /// bypasses the default public bridge.
        #[arg(long, value_name = "URL", requires = "web")]
        web_url: Option<String>,
    },
    /// Inspect clipboard copy and paste capabilities without reading clipboard contents.
    Clipboard {
        #[command(subcommand)]
        command: ClipboardCommands,
    },
    /// Inspect or test native metadata-only attention notifications.
    Notifications {
        #[command(subcommand)]
        command: NotificationCommands,
    },
    /// List or revoke devices paired with this daemon.
    Device {
        #[command(subcommand)]
        command: DeviceCommands,
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
    Check {
        /// Watch for this many seconds so a file can be copied after the
        /// command starts, for when getting the command into a terminal is
        /// what overwrote the clipboard.
        #[arg(long, value_name = "SECONDS")]
        wait: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, Subcommand)]
enum NotificationCommands {
    /// Report configured notification filters and desktop delivery capability.
    Check,
    /// Enable native notifications in the configuration.
    Enable,
    /// Disable native notifications in the configuration.
    Disable,
    /// Deliver one synthetic metadata-only desktop notification.
    Test,
}

#[derive(Debug, Subcommand)]
enum DeviceCommands {
    /// List paired devices without revealing anything they hold.
    List,
    /// Revoke a device. What it holds stops working at once.
    Remove {
        /// Name shown by `device list`.
        name: String,
    },
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
    /// Edit one configured Herdr host.
    Edit {
        /// Current stable target name.
        name: String,
        /// Replace the stable target name.
        #[arg(long, value_name = "NAME")]
        rename: Option<String>,
        /// Replace the OpenSSH destination or alias.
        #[arg(long, value_name = "DESTINATION", conflicts_with = "local")]
        ssh: Option<String>,
        /// Change this to a local target.
        #[arg(long, conflicts_with = "ssh")]
        local: bool,
        /// Discover every running Herdr session.
        #[arg(long, conflicts_with = "single_session")]
        discover_sessions: bool,
        /// Disable session discovery and use one session.
        #[arg(long, conflicts_with = "discover_sessions")]
        single_session: bool,
        /// Replace the named session or discovery fallback.
        #[arg(long, value_name = "NAME", conflicts_with = "clear_session")]
        session: Option<String>,
        /// Remove an explicitly configured session fallback.
        #[arg(long)]
        clear_session: bool,
        /// Replace the Herdr API socket path.
        #[arg(long, value_name = "PATH", conflicts_with = "clear_socket")]
        socket: Option<String>,
        /// Remove an explicitly configured socket path.
        #[arg(long)]
        clear_socket: bool,
        /// Replace client candidates; repeat to define fallback order.
        #[arg(
            long = "herdr-bin",
            value_name = "COMMAND",
            conflicts_with = "default_herdr_bin"
        )]
        herdr_bins: Vec<String>,
        /// Reset the client candidate list to `herdr`.
        #[arg(long)]
        default_herdr_bin: bool,
    },
    /// Remove a configured host without touching its Herdr sessions.
    Remove {
        /// Stable target name to remove.
        name: String,
        /// Confirm removal from Super-Herdr configuration.
        #[arg(long)]
        yes: bool,
    },
    /// List configured Herdr hosts without connecting to them.
    List,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(error) if is_broken_pipe(&error) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
    })
}

fn stdout_line(arguments: fmt::Arguments<'_>) -> Result<()> {
    let mut output = io::stdout().lock();
    output
        .write_fmt(arguments)
        .context("failed to write standard output")?;
    output
        .write_all(b"\n")
        .context("failed to write standard output")
}

async fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Commands::Tui);
    if let Commands::Clipboard {
        command: ClipboardCommands::Check { wait },
    } = &command
    {
        let wait =
            wait.map(|seconds| Duration::from_secs(seconds).min(clipboard::MAXIMUM_PROBE_WAIT));
        if let Some(wait) = wait {
            stdout_line(format_args!(
                "watching the clipboard for {} seconds; copy a file now",
                wait.as_secs()
            ))?;
        }
        for line in clipboard::diagnostic_lines(wait).await {
            stdout_line(format_args!("{line}"))?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    let command = match command {
        Commands::Target { command } => {
            return run_target_command(cli.config.as_deref(), command);
        }
        Commands::Device { command } => {
            return run_device_command(cli.config.as_deref(), command);
        }
        command => command,
    };
    let (config, path) = Config::load(cli.config.as_deref())?;

    match command {
        Commands::Check => {
            stdout_line(format_args!(
                "{}: {} valid target(s)",
                path.display(),
                config.targets.len()
            ))?;
            for target in &config.targets {
                let session_scope = if target.discover_sessions {
                    format!("discover sessions (fallback {})", target.session_name())
                } else {
                    format!("session {}", target.session_name())
                };
                stdout_line(format_args!(
                    "  {}: {} / {} / {}",
                    target.name,
                    target.endpoint(),
                    session_scope,
                    target.candidate_bins().collect::<Vec<_>>().join(", ")
                ))?;
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
                stdout_line(format_args!(
                    "{}",
                    serde_json::to_string_pretty(&FederationReport {
                        config: path.display().to_string(),
                        targets: reports,
                    })?
                ))?;
            } else {
                let color = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
                for report in &reports {
                    if report.ok {
                        stdout_line(format_args!(
                            "{}{}/{} [{}] Herdr {} protocol {} via {}: {} workspace(s), {} pane(s), {} agent(s) ({} ms)",
                            probe_status_prefix(true, color),
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
                        ))?;
                    } else {
                        stdout_line(format_args!(
                            "{}{}/{} [{}]: {} ({} ms)",
                            probe_status_prefix(false, color),
                            report.target,
                            report.session,
                            report.endpoint,
                            report.error.as_deref().unwrap_or("unknown error"),
                            report.elapsed_ms
                        ))?;
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
            tui::run(config, path).await?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Daemon {
            socket,
            web,
            web_address,
            web_url,
        } => {
            let mut options = DaemonOptions::discover()?;
            if let Some(socket) = socket {
                options.socket = socket;
            }
            // A flag overrides the file; absent, the file is what the frontend
            // and this subcommand agree on, so a machine's own address is
            // stated once rather than retyped per invocation.
            let mut web_config = config.web.clone();
            if let Some(port) = web {
                web_config.port = Some(port);
            }
            if let Some(address) = web_address {
                web_config.address = Some(address);
            }
            if let Some(url) = web_url {
                web_config.url = Some(super_herdr::pairing::pairing_url(&url)?);
            }
            let resolved_web = web_config.resolve().await;
            options.web_port = resolved_web.port;
            options.web_address = resolved_web.address;
            options.web_url = resolved_web.url;
            options.web_bridge = resolved_web.bridge;
            // The daemon announces itself once it is actually listening; a
            // line printed here would be a claim rather than a report.
            serve(config, Some(path), options).await?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Clipboard { .. } => unreachable!("clipboard commands return before config load"),
        Commands::Notifications { command } => {
            match command {
                NotificationCommands::Check => {
                    for line in notifications::diagnostic_lines(&config.notifications).await {
                        stdout_line(format_args!("{line}"))?;
                    }
                }
                NotificationCommands::Enable | NotificationCommands::Disable => {
                    let enabled = matches!(command, NotificationCommands::Enable);
                    Config::set_notifications_enabled_file(Some(&path), enabled)?;
                    stdout_line(format_args!(
                        "native notifications {} in {}",
                        if enabled { "enabled" } else { "disabled" },
                        path.display()
                    ))?;
                    stdout_line(format_args!(
                        "the TUI will reload this setting without restarting Herdr"
                    ))?;
                }
                NotificationCommands::Test => {
                    let delivery = notifications::test_delivery(&config.notifications)?;
                    notifications::deliver(delivery).await?;
                    stdout_line(format_args!("native test notification delivered"))?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Target { .. } => unreachable!("target commands return before config load"),
        Commands::Device { .. } => unreachable!("device commands return before config load"),
    }
}

fn run_device_command(
    config_path: Option<&std::path::Path>,
    command: DeviceCommands,
) -> Result<ExitCode> {
    match command {
        DeviceCommands::List => {
            let (config, path) = Config::load(config_path)?;
            stdout_line(format_args!(
                "{}: {} paired device(s)",
                path.display(),
                config.devices.len()
            ))?;
            for device in &config.devices {
                // The digest is shown truncated: enough to tell two entries
                // apart, and useless to anyone who reads it over a shoulder.
                stdout_line(format_args!(
                    "  {}: paired {} (…{})",
                    device.name,
                    device.paired_at_ms,
                    &device.token_sha256[device.token_sha256.len().saturating_sub(8)..]
                ))?;
            }
            if config.devices.is_empty() {
                stdout_line(format_args!(
                    "the browser client serves loopback only until a device is paired"
                ))?;
            }
            Ok(ExitCode::SUCCESS)
        }
        DeviceCommands::Remove { name } => {
            let path = Config::remove_device_file(config_path, &name)?;
            stdout_line(format_args!(
                "revoked device {name:?} in {}",
                path.display()
            ))?;
            stdout_line(format_args!(
                "it stops working at that device's next request; no Herdr session was touched"
            ))?;
            Ok(ExitCode::SUCCESS)
        }
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
                // Not offered on the command line. A browsing bound is a
                // considered decision about a host, written where it can be
                // read back, rather than a flag typed once while adding one.
                roots: Vec::new(),
            };
            let path = Config::add_target_file(config_path, target)?;
            stdout_line(format_args!("added target {name:?} to {}", path.display()))?;
            stdout_line(format_args!("run `super-herdr probe` to verify it"))?;
            Ok(ExitCode::SUCCESS)
        }
        TargetCommands::Edit {
            name,
            rename,
            ssh,
            local,
            discover_sessions,
            single_session,
            session,
            clear_session,
            socket,
            clear_socket,
            herdr_bins,
            default_herdr_bin,
        } => {
            let (config, _) = Config::load(config_path)?;
            let mut target = config
                .targets
                .iter()
                .find(|target| target.name == name)
                .cloned()
                .with_context(|| format!("target {name:?} is not configured"))?;
            let changed = rename.is_some()
                || ssh.is_some()
                || local
                || discover_sessions
                || single_session
                || session.is_some()
                || clear_session
                || socket.is_some()
                || clear_socket
                || !herdr_bins.is_empty()
                || default_herdr_bin;
            if !changed {
                anyhow::bail!("target edit requires at least one change option");
            }
            if let Some(rename) = rename {
                target.name = rename;
            }
            if let Some(ssh) = ssh {
                target.ssh = Some(ssh);
            } else if local {
                target.ssh = None;
            }
            if discover_sessions {
                target.discover_sessions = true;
            } else if single_session {
                target.discover_sessions = false;
            }
            if let Some(session) = session {
                target.session = Some(session);
            } else if clear_session {
                target.session = None;
            }
            if let Some(socket) = socket {
                target.socket = Some(socket);
            } else if clear_socket {
                target.socket = None;
            }
            if !herdr_bins.is_empty() {
                target.herdr_bins = herdr_bins;
            } else if default_herdr_bin {
                target.herdr_bins = vec!["herdr".to_owned()];
            }
            let updated_name = target.name.clone();
            let path = Config::replace_target_file(config_path, &name, target)?;
            stdout_line(format_args!(
                "updated target {name:?} as {updated_name:?} in {}",
                path.display()
            ))?;
            stdout_line(format_args!("run `super-herdr probe` to verify it"))?;
            Ok(ExitCode::SUCCESS)
        }
        TargetCommands::Remove { name, yes } => {
            if !yes {
                anyhow::bail!(
                    "refusing to remove target {name:?} without --yes; Herdr is not affected"
                );
            }
            let path = Config::remove_target_file(config_path, &name)?;
            stdout_line(format_args!(
                "removed target {name:?} from {}",
                path.display()
            ))?;
            stdout_line(format_args!("no Herdr session was stopped or restarted"))?;
            Ok(ExitCode::SUCCESS)
        }
        TargetCommands::List => {
            let (config, path) = Config::load(config_path)?;
            stdout_line(format_args!(
                "{}: {} configured target(s)",
                path.display(),
                config.targets.len()
            ))?;
            for target in &config.targets {
                let scope = if target.discover_sessions {
                    "all running sessions"
                } else {
                    target.session_name()
                };
                stdout_line(format_args!(
                    "  {}: {} / {scope}",
                    target.name,
                    target.endpoint()
                ))?;
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_broken_pipe, probe_status_prefix};

    #[test]
    fn recognizes_a_contextualized_broken_pipe() {
        let error = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed pipe",
        ))
        .context("output failed");
        assert!(is_broken_pipe(&error));
        assert!(!is_broken_pipe(&anyhow::anyhow!("ordinary failure")));
    }

    #[test]
    fn probe_status_prefixes_are_colored_only_when_requested() {
        assert_eq!(probe_status_prefix(true, false), "OK   ");
        assert_eq!(probe_status_prefix(false, false), "FAIL ");
        assert_eq!(probe_status_prefix(true, true), "\x1b[32mOK\x1b[0m   ");
        assert_eq!(probe_status_prefix(false, true), "\x1b[31mFAIL\x1b[0m ");
    }
}
