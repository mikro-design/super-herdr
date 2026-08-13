use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub transport: TransportConfig,
    pub targets: Vec<Target>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportConfig {
    #[serde(default = "default_ssh_bin")]
    pub ssh_bin: String,
    #[serde(default = "default_true")]
    pub batch_mode: bool,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_seconds: u64,
    #[serde(default = "default_command_timeout")]
    pub command_timeout_seconds: u64,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            ssh_bin: default_ssh_bin(),
            batch_mode: true,
            connect_timeout_seconds: default_connect_timeout(),
            command_timeout_seconds: default_command_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
    /// Discover all Herdr sessions on this host at startup.
    #[serde(default)]
    pub discover_sessions: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Documented Herdr API socket. Enables event subscriptions when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    /// Ordered client candidates. A protocol mismatch advances to the next one.
    #[serde(default = "default_herdr_bins")]
    pub herdr_bins: Vec<String>,
}

impl Target {
    pub fn endpoint(&self) -> &str {
        self.ssh.as_deref().unwrap_or("local")
    }

    pub fn session_name(&self) -> &str {
        self.session.as_deref().unwrap_or("default")
    }

    pub fn candidate_bins(&self) -> impl Iterator<Item = &str> {
        self.herdr_bins.iter().map(String::as_str)
    }
}

impl Config {
    pub fn load(explicit_path: Option<&Path>) -> Result<(Self, PathBuf)> {
        let path = resolve_path(explicit_path)?;
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config =
            Self::parse(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        Ok((config, path))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn add_target_file(explicit_path: Option<&Path>, target: Target) -> Result<PathBuf> {
        let path = resolve_path(explicit_path)?;
        let existing = match fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };

        let contents = if let Some(mut text) = existing {
            let mut config = Self::parse(&text)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            config.targets.push(target.clone());
            config.validate()?;

            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push('\n');
            text.push_str(&toml::to_string_pretty(&TargetAppend {
                targets: vec![&target],
            })?);
            text
        } else {
            let config = Self {
                transport: TransportConfig::default(),
                targets: vec![target],
            };
            config.validate()?;
            toml::to_string_pretty(&config)?
        };

        Self::parse(&contents).context("generated configuration is invalid")?;
        write_private_atomic(&path, contents.as_bytes())?;
        Ok(path)
    }

    fn validate(&self) -> Result<()> {
        if self.targets.is_empty() {
            bail!("configuration must contain at least one [[targets]] entry");
        }
        if self.transport.ssh_bin.trim().is_empty() {
            bail!("transport.ssh_bin must not be empty");
        }
        if self.transport.connect_timeout_seconds == 0 {
            bail!("transport.connect_timeout_seconds must be greater than zero");
        }
        if self.transport.command_timeout_seconds == 0 {
            bail!("transport.command_timeout_seconds must be greater than zero");
        }

        let mut names = HashSet::new();
        for target in &self.targets {
            if target.name.trim().is_empty()
                || target.name.contains('/')
                || target.name.chars().any(char::is_control)
            {
                bail!("target name must be non-empty and contain no '/' or control characters");
            }
            if !names.insert(target.name.as_str()) {
                bail!("duplicate target name {:?}", target.name);
            }
            if target.herdr_bins.is_empty()
                || target
                    .herdr_bins
                    .iter()
                    .any(|binary| binary.trim().is_empty())
            {
                bail!("target {:?} has an empty herdr_bins entry", target.name);
            }
            if let Some(destination) = &target.ssh
                && (destination.is_empty()
                    || destination.starts_with('-')
                    || destination.chars().any(char::is_whitespace)
                    || destination.chars().any(char::is_control))
            {
                bail!("target {:?} has an invalid SSH destination", target.name);
            }
            if target.session.as_ref().is_some_and(|session| {
                session.is_empty() || session.contains('/') || session.chars().any(char::is_control)
            }) {
                bail!("target {:?} has an invalid session name", target.name);
            }
            if let Some(socket) = &target.socket {
                if socket.is_empty()
                    || !Path::new(socket).is_absolute()
                    || socket.chars().any(char::is_control)
                {
                    bail!("target {:?} has an invalid socket path", target.name);
                }
                if target.ssh.is_some() && socket.contains(':') {
                    bail!(
                        "target {:?} has a socket path incompatible with SSH forwarding",
                        target.name
                    );
                }
            }
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct TargetAppend<'a> {
    targets: Vec<&'a Target>,
}

fn write_private_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let directory = path
        .parent()
        .context("configuration path has no parent directory")?;
    let directory_exists = directory.exists();
    fs::create_dir_all(directory).context("failed to create the configuration directory")?;
    if !directory_exists {
        set_directory_permissions(directory)?;
    }
    let mut temporary = tempfile::Builder::new()
        .prefix(".config-")
        .tempfile_in(directory)
        .context("failed to create a temporary configuration file")?;
    set_file_permissions(temporary.path())?;
    temporary
        .write_all(contents)
        .context("failed to write configuration")?;
    temporary
        .as_file()
        .sync_all()
        .context("failed to synchronize configuration")?;
    temporary
        .persist(path)
        .context("failed to atomically replace configuration")?;
    Ok(())
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .context("failed to secure the configuration directory")
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .context("failed to secure the configuration file")
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn resolve_path(explicit_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit_path {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("SUPER_HERDR_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    if let Some(root) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(root).join("super-herdr/config.toml"));
    }
    let home: OsString = std::env::var_os("HOME")
        .context("no --config, SUPER_HERDR_CONFIG, XDG_CONFIG_HOME, or HOME was set")?;
    Ok(PathBuf::from(home).join(".config/super-herdr/config.toml"))
}

fn default_ssh_bin() -> String {
    "ssh".to_owned()
}

fn default_herdr_bins() -> Vec<String> {
    vec!["herdr".to_owned()]
}

const fn default_true() -> bool {
    true
}

const fn default_connect_timeout() -> u64 {
    10
}

const fn default_command_timeout() -> u64 {
    20
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{Config, Target};

    #[test]
    fn parses_local_and_ssh_targets() {
        let config = Config::parse(
            r#"
                [[targets]]
                name = "local"

                [[targets]]
                name = "remote"
                ssh = "host-alias"
                session = "dev"
                socket = "/srv/herdr/sessions/dev/herdr.sock"
            "#,
        )
        .unwrap();

        assert_eq!(config.targets[0].endpoint(), "local");
        assert_eq!(config.targets[1].endpoint(), "host-alias");
        assert!(!config.targets[0].discover_sessions);
        assert_eq!(config.targets[1].session_name(), "dev");
        assert_eq!(
            config.targets[1].socket.as_deref(),
            Some("/srv/herdr/sessions/dev/herdr.sock")
        );
        assert_eq!(
            config.targets[1].candidate_bins().collect::<Vec<_>>(),
            ["herdr"]
        );
    }

    #[test]
    fn enables_host_session_discovery_explicitly() {
        let config = Config::parse(
            r#"
                [[targets]]
                name = "host"
                discover_sessions = true
            "#,
        )
        .unwrap();

        assert!(config.targets[0].discover_sessions);
    }

    #[test]
    fn preserves_client_candidate_order() {
        let config = Config::parse(
            r#"
                [[targets]]
                name = "mixed"
                herdr_bins = ["herdr-0.8", "herdr-0.7"]
            "#,
        )
        .unwrap();

        assert_eq!(
            config.targets[0].candidate_bins().collect::<Vec<_>>(),
            ["herdr-0.8", "herdr-0.7"]
        );
    }

    #[test]
    fn rejects_duplicate_target_names() {
        let error = Config::parse(
            r#"
                [[targets]]
                name = "same"
                [[targets]]
                name = "same"
            "#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("duplicate target"));
    }

    #[test]
    fn rejects_ssh_option_injection() {
        let error = Config::parse(
            r#"
                [[targets]]
                name = "bad"
                ssh = "-oProxyCommand=oops"
            "#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid SSH destination"));
    }

    #[test]
    fn rejects_relative_event_socket_paths() {
        let error = Config::parse(
            r#"
                [[targets]]
                name = "bad"
                socket = "relative/herdr.sock"
            "#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid socket path"));
    }

    #[test]
    fn rejects_ambiguous_qualified_names() {
        let error = Config::parse(
            r#"
                [[targets]]
                name = "bad/name"
            "#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("target name"));
    }

    #[test]
    fn atomically_creates_and_appends_targets_without_losing_comments() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let first = Target {
            name: "development".to_owned(),
            ssh: Some("development-host".to_owned()),
            discover_sessions: true,
            session: None,
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
        };
        Config::add_target_file(Some(&path), first).unwrap();

        let mut text = fs::read_to_string(&path).unwrap();
        text.insert_str(0, "# retained operator note\n");
        fs::write(&path, text).unwrap();

        let second = Target {
            name: "build".to_owned(),
            ssh: Some("build-host".to_owned()),
            discover_sessions: false,
            session: Some("toolchains".to_owned()),
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
        };
        Config::add_target_file(Some(&path), second).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# retained operator note\n"));
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.targets.len(), 2);
        assert_eq!(config.targets[1].name, "build");
    }

    #[test]
    fn rejects_duplicate_target_add_without_changing_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let target = Target {
            name: "development".to_owned(),
            ssh: Some("development-host".to_owned()),
            discover_sessions: true,
            session: None,
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
        };
        Config::add_target_file(Some(&path), target.clone()).unwrap();
        let before = fs::read(&path).unwrap();

        let error = Config::add_target_file(Some(&path), target).unwrap_err();

        assert!(error.to_string().contains("duplicate target"));
        assert_eq!(fs::read(path).unwrap(), before);
    }
}
