use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub transport: TransportConfig,
    #[serde(default, skip_serializing_if = "NotificationsConfig::is_default")]
    pub notifications: NotificationsConfig,
    #[serde(default, skip_serializing_if = "TransferConfig::is_default")]
    pub transfers: TransferConfig,
    #[serde(default, skip_serializing_if = "WebConfig::is_default")]
    pub web: WebConfig,
    pub targets: Vec<Target>,
    /// Devices allowed to reach this daemon over a network. Empty means none,
    /// which is why a daemon with no paired device serves loopback only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<Device>,
}

/// What the daemon will move on someone's behalf.
///
/// The ceiling is a statement about the target's disk rather than about this
/// process: a transfer is relayed as it arrives, so nothing here grows with the
/// file. It is configurable because the right answer belongs to whoever owns
/// the host being written to, and it is separate from the clipboard's own limit
/// because a screenshot and a file are not the same question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferConfig {
    #[serde(default = "default_max_transfer_bytes")]
    pub max_bytes: u64,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            max_bytes: default_max_transfer_bytes(),
        }
    }
}

impl TransferConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

impl WebConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Where a device reaches the browser client, given or worked out.
    ///
    /// Worked out, because asking somebody to write down a URL for a listener
    /// they have already located is a question with an answer the program is
    /// holding: bound to a private address on a known port, the URL is that
    /// address on that port and nothing else.
    ///
    /// Not derivable from loopback, which no other device can reach, and not
    /// derivable through a proxy that terminates TLS somewhere else — that is
    /// what `url` is for, and it stays the override.
    pub fn scannable_url(&self) -> Option<String> {
        if let Some(url) = self.url.clone() {
            return Some(url);
        }
        let (address, port) = (self.address?, self.port?);
        if address.is_loopback() || !crate::daemon::web::bindable(address) {
            return None;
        }
        Some(match address {
            std::net::IpAddr::V4(address) => format!("http://{address}:{port}"),
            std::net::IpAddr::V6(address) => format!("http://[{address}]:{port}"),
        })
    }
}

/// One paired device.
///
/// The secret itself is never here: a configuration file that held working
/// credentials would make a backup of it a way in. Revoking is deleting the
/// entry, which cannot be undone by anything the device still holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    /// What a person calls it when deciding whether to revoke it.
    pub name: String,
    /// SHA-256 of the device's token, as lowercase hex.
    pub token_sha256: String,
    pub paired_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Where the browser client is served, and where a phone reaches it.
///
/// These lived only on `super-herdr daemon`, which meant the frontend — the way
/// the program is normally run — hosted a daemon that served no browser client
/// and knew no address. Pairing from there could only ever offer a code to be
/// typed into a client that was not running, and a QR needs an address, so
/// there was never one to show. A configuration file is where a machine's own
/// address belongs anyway: it does not change between runs, and retyping it as
/// a flag every time is how it ends up not being set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// Serve the browser client on this port. `None` serves none at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// What to bind. Loopback by default; a private or mesh address lets a
    /// paired device reach it directly, and a public one is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<std::net::IpAddr>,
    /// Where a device outside this machine reaches it. Never derived from the
    /// bind: behind a proxy that terminates TLS the host, port and scheme a
    /// phone needs are all different from what this process listens on, and a
    /// derived one would be a perfectly valid code for an unreachable address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub needs_attention: bool,
    #[serde(default = "default_true")]
    pub completed: bool,
    #[serde(default = "default_true")]
    pub disappeared: bool,
    #[serde(default)]
    pub working: bool,
    #[serde(default)]
    pub status_changed: bool,
    #[serde(default = "default_notification_interval")]
    pub minimum_interval_seconds: u64,
    #[serde(default = "default_notification_timeout")]
    pub command_timeout_seconds: u64,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            needs_attention: true,
            completed: true,
            disappeared: true,
            working: false,
            status_changed: false,
            minimum_interval_seconds: default_notification_interval(),
            command_timeout_seconds: default_notification_timeout(),
        }
    }
}

impl NotificationsConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
                notifications: NotificationsConfig::default(),
                transfers: TransferConfig::default(),
                web: Default::default(),
                targets: vec![target],
                devices: Vec::new(),
            };
            config.validate()?;
            toml::to_string_pretty(&config)?
        };

        Self::parse(&contents).context("generated configuration is invalid")?;
        write_private_atomic(&path, contents.as_bytes())?;
        Ok(path)
    }

    pub fn replace_target_file(
        explicit_path: Option<&Path>,
        existing_name: &str,
        replacement: Target,
    ) -> Result<PathBuf> {
        mutate_target_file(explicit_path, existing_name, Some(replacement))
    }

    pub fn remove_target_file(explicit_path: Option<&Path>, name: &str) -> Result<PathBuf> {
        mutate_target_file(explicit_path, name, None)
    }

    /// Record a paired device, keeping every existing line of the file.
    pub fn add_device_file(explicit_path: Option<&Path>, device: Device) -> Result<PathBuf> {
        let path = resolve_path(explicit_path)?;
        let mut text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config =
            Self::parse(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        if config
            .devices
            .iter()
            .any(|existing| existing.name == device.name)
        {
            anyhow::bail!("a device named {:?} is already paired", device.name);
        }
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push('\n');
        text.push_str(&toml::to_string_pretty(&DeviceAppend {
            devices: vec![&device],
        })?);
        Self::parse(&text).context("generated configuration is invalid")?;
        write_private_atomic(&path, text.as_bytes())?;
        Ok(path)
    }

    /// Revoke a device. What it holds stops working immediately, because the
    /// daemon has nothing left to compare it against.
    pub fn remove_device_file(explicit_path: Option<&Path>, name: &str) -> Result<PathBuf> {
        let path = resolve_path(explicit_path)?;
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut config =
            Self::parse(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        let before = config.devices.len();
        config.devices.retain(|device| device.name != name);
        if config.devices.len() == before {
            anyhow::bail!("no device named {name:?} is paired");
        }
        let updated = remove_device_block(&text, name);
        let parsed = Self::parse(&updated).context("generated configuration is invalid")?;
        anyhow::ensure!(
            parsed.devices.len() == config.devices.len(),
            "removing the device would have changed more than its own entry"
        );
        write_private_atomic(&path, updated.as_bytes())?;
        Ok(path)
    }

    pub fn set_notifications_enabled_file(
        explicit_path: Option<&Path>,
        enabled: bool,
    ) -> Result<PathBuf> {
        let path = resolve_path(explicit_path)?;
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        let updated = set_notifications_enabled(&text, enabled);
        Self::parse(&updated).context("generated configuration is invalid")?;
        write_private_atomic(&path, updated.as_bytes())?;
        Ok(path)
    }

    pub fn validate(&self) -> Result<()> {
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
        // Checked here rather than where it is served, so a configuration file
        // that cannot work says so when it is read instead of when somebody
        // finally tries to pair a phone.
        if let Some(url) = self.web.url.as_deref() {
            crate::pairing::pairing_url(url)?;
        }
        if self.web.url.is_some() && self.web.port.is_none() {
            bail!("web.url names an address for a browser client that web.port does not serve");
        }
        if self.web.address.is_some() && self.web.port.is_none() {
            bail!("web.address binds a browser client that web.port does not serve");
        }

        if self.notifications.minimum_interval_seconds == 0
            || self.notifications.minimum_interval_seconds > 3600
        {
            bail!("notifications.minimum_interval_seconds must be between 1 and 3600");
        }
        if self.notifications.command_timeout_seconds == 0
            || self.notifications.command_timeout_seconds > 30
        {
            bail!("notifications.command_timeout_seconds must be between 1 and 30");
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

#[derive(Deserialize)]
struct OwnedTargetAppend {
    targets: Vec<Target>,
}

fn mutate_target_file(
    explicit_path: Option<&Path>,
    existing_name: &str,
    replacement: Option<Target>,
) -> Result<PathBuf> {
    let path = resolve_path(explicit_path)?;
    let text =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut config =
        Config::parse(&text).with_context(|| format!("failed to parse {}", path.display()))?;
    let index = config
        .targets
        .iter()
        .position(|target| target.name == existing_name)
        .with_context(|| format!("target {existing_name:?} is not configured"))?;

    match replacement.as_ref() {
        Some(target) => config.targets[index] = target.clone(),
        None if config.targets.len() == 1 => {
            bail!("cannot remove the final configured target");
        }
        None => {
            config.targets.remove(index);
        }
    }
    config.validate()?;

    let range = target_section(&text, existing_name)?;
    let replacement_text = replacement
        .as_ref()
        .map(|target| {
            toml::to_string_pretty(&TargetAppend {
                targets: vec![target],
            })
        })
        .transpose()?;
    let mut updated = String::with_capacity(
        text.len() - range.len() + replacement_text.as_ref().map_or(0, String::len),
    );
    updated.push_str(&text[..range.start]);
    if let Some(replacement_text) = replacement_text {
        updated.push_str(&replacement_text);
    }
    updated.push_str(&text[range.end..]);
    Config::parse(&updated).context("generated configuration is invalid")?;
    write_private_atomic(&path, updated.as_bytes())?;
    Ok(path)
}

fn target_section(text: &str, name: &str) -> Result<Range<usize>> {
    for range in target_sections(text) {
        let section = &text[range.clone()];
        let Ok(parsed) = toml::from_str::<OwnedTargetAppend>(section) else {
            continue;
        };
        if parsed.targets.len() == 1 && parsed.targets[0].name == name {
            return Ok(range);
        }
    }
    bail!("could not locate target {name:?} in the configuration text")
}

fn target_sections(text: &str) -> Vec<Range<usize>> {
    let mut headers = Vec::new();
    let mut offset = 0_usize;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            headers.push((offset, trimmed == "[[targets]]"));
        }
        offset += line.len();
    }
    headers
        .iter()
        .enumerate()
        .filter(|(_, (_, is_target))| *is_target)
        .map(|(index, (start, _))| {
            let end = headers
                .get(index + 1)
                .map(|(offset, _)| *offset)
                .unwrap_or(text.len());
            *start..end
        })
        .collect()
}

fn set_notifications_enabled(text: &str, enabled: bool) -> String {
    let value = if enabled { "true" } else { "false" };
    let headers = table_headers(text);
    if let Some((header_index, (start, _))) = headers
        .iter()
        .enumerate()
        .find(|(_, (_, header))| *header == "[notifications]")
    {
        let end = headers
            .get(header_index + 1)
            .map(|(offset, _)| *offset)
            .unwrap_or(text.len());
        let section = &text[*start..end];
        let mut offset = 0_usize;
        for line in section.split_inclusive('\n') {
            let trimmed = line.trim();
            if trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "enabled")
            {
                let line_start = start + offset;
                let line_end = line_start + line.len();
                let (content, newline) = line
                    .strip_suffix('\n')
                    .map_or((line, ""), |content| (content, "\n"));
                let equals = content
                    .find('=')
                    .expect("an enabled assignment always contains '='");
                let right = &content[equals + 1..];
                let value_start = right
                    .find(|character: char| !character.is_whitespace())
                    .unwrap_or(right.len());
                let before_comment = right.find('#').unwrap_or(right.len());
                let value_end = right[..before_comment].trim_end().len();
                let mut updated = String::with_capacity(text.len());
                updated.push_str(&text[..line_start]);
                updated.push_str(&content[..equals + 1]);
                updated.push_str(&right[..value_start]);
                updated.push_str(value);
                updated.push_str(&right[value_end..]);
                updated.push_str(newline);
                updated.push_str(&text[line_end..]);
                return updated;
            }
            offset += line.len();
        }
        let header_end = text[*start..]
            .find('\n')
            .map(|offset| start + offset + 1)
            .unwrap_or(text.len());
        let mut updated = String::with_capacity(text.len() + 16);
        updated.push_str(&text[..header_end]);
        updated.push_str(&format!("enabled = {value}\n"));
        updated.push_str(&text[header_end..]);
        return updated;
    }

    let insert_at = headers
        .iter()
        .find(|(_, header)| *header == "[[targets]]")
        .map(|(offset, _)| *offset)
        .unwrap_or(text.len());
    let mut updated = String::with_capacity(text.len() + 42);
    updated.push_str(&text[..insert_at]);
    if insert_at > 0 && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("[notifications]\nenabled = {value}\n\n"));
    updated.push_str(&text[insert_at..]);
    updated
}

fn table_headers(text: &str) -> Vec<(usize, &str)> {
    let mut headers = Vec::new();
    let mut offset = 0_usize;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            headers.push((offset, trimmed));
        }
        offset += line.len();
    }
    headers
}

#[derive(Serialize)]
struct DeviceAppend<'a> {
    devices: Vec<&'a Device>,
}

/// Drop one `[[devices]]` block, leaving every other line as it was.
///
/// Rewriting the file from the parsed model would silently discard comments a
/// person put there, so the text is edited instead and the result is parsed
/// back to prove only the intended entry left.
fn remove_device_block(text: &str, name: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut in_target_block = false;
    let mut dropping = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_target_block = trimmed == "[[devices]]";
            dropping = false;
        }
        if in_target_block
            && trimmed.starts_with("name")
            && let Some((_, value)) = trimmed.split_once('=')
        {
            {
                dropping = value.trim().trim_matches('"') == name;
                if dropping {
                    // Remove the header line that was already written.
                    while kept.ends_with('\n') {
                        kept.pop();
                    }
                    let header = kept.rfind("[[devices]]").unwrap_or(kept.len());
                    kept.truncate(header);
                }
            }
        }
        if dropping {
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    while kept.ends_with("\n\n") {
        kept.pop();
    }
    kept
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

/// One gibibyte. Large enough that a build artifact or a core dump is a
/// transfer rather than a refusal, small enough that a mistyped length does not
/// fill a host's disk before anyone notices.
const fn default_max_transfer_bytes() -> u64 {
    1024 * 1024 * 1024
}

const fn default_notification_interval() -> u64 {
    5
}

const fn default_notification_timeout() -> u64 {
    5
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{Config, Device, Target};

    #[test]
    fn a_transfer_ceiling_is_configurable_and_has_a_default() {
        // Absent is the common case, and it must not mean zero: a missing
        // section that refused every transfer would be a silent outage.
        let default = Config::parse(
            r#"
                [[targets]]
                name = "local"
            "#,
        )
        .unwrap();
        assert_eq!(default.transfers.max_bytes, 1024 * 1024 * 1024);

        let configured = Config::parse(
            r#"
                [transfers]
                max_bytes = 4096

                [[targets]]
                name = "local"
            "#,
        )
        .unwrap();
        assert_eq!(configured.transfers.max_bytes, 4096);

        // The documented example is parsed rather than assumed to be current:
        // a configuration file nobody reads back drifts from the code.
        let example = Config::parse(&fs::read_to_string("config.example.toml").unwrap()).unwrap();
        assert_eq!(example.transfers.max_bytes, 1024 * 1024 * 1024);
    }

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
    fn notifications_are_opt_in_and_validate_delivery_bounds() {
        let disabled = Config::parse(
            r#"
                [[targets]]
                name = "local"
            "#,
        )
        .unwrap();
        assert!(!disabled.notifications.enabled);
        assert!(disabled.notifications.needs_attention);

        let enabled = Config::parse(
            r#"
                [notifications]
                enabled = true
                minimum_interval_seconds = 7
                completed = false

                [[targets]]
                name = "local"
            "#,
        )
        .unwrap();
        assert!(enabled.notifications.enabled);
        assert_eq!(enabled.notifications.minimum_interval_seconds, 7);
        assert!(!enabled.notifications.completed);
        assert!(enabled.notifications.disappeared);

        let error = Config::parse(
            r#"
                [notifications]
                enabled = true
                minimum_interval_seconds = 0

                [[targets]]
                name = "local"
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("minimum_interval_seconds"));
    }

    #[test]
    fn notification_toggle_preserves_comments_and_filter_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"# retained operator note
[notifications]
completed = false

[[targets]]
name = "local"
"#,
        )
        .unwrap();

        Config::set_notifications_enabled_file(Some(&path), true).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# retained operator note\n"));
        assert!(text.contains("enabled = true"));
        assert!(text.contains("completed = false"));
        let config = Config::parse(&text).unwrap();
        assert!(config.notifications.enabled);
        assert!(!config.notifications.completed);

        fs::write(
            &path,
            text.replace("enabled = true", "enabled = true # retained switch note"),
        )
        .unwrap();
        Config::set_notifications_enabled_file(Some(&path), false).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("enabled = false # retained switch note"));
        assert!(!Config::parse(&text).unwrap().notifications.enabled);

        let inserted_path = directory.path().join("without-notifications.toml");
        Config::add_target_file(
            Some(&inserted_path),
            Target {
                name: "inserted".to_owned(),
                ssh: None,
                discover_sessions: false,
                session: None,
                socket: None,
                herdr_bins: vec!["herdr".to_owned()],
            },
        )
        .unwrap();
        Config::set_notifications_enabled_file(Some(&inserted_path), true).unwrap();
        let inserted = fs::read_to_string(inserted_path).unwrap();
        assert!(inserted.contains("[notifications]\nenabled = true"));
        assert!(Config::parse(&inserted).unwrap().notifications.enabled);
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

    /// The URL a phone needs is not a question worth asking somebody who has
    /// already said where the listener is. Bound to a private address on a
    /// known port, the answer is that address on that port.
    #[test]
    fn a_scannable_url_is_worked_out_from_the_bind_when_one_is_not_given() {
        let web = |table: &str| {
            Config::parse(&format!(
                r#"
                {table}
                [[targets]]
                name = "development"
                ssh = "development-host"
            "#
            ))
            .map(|config| config.web)
        };

        // A LAN address is reachable and derivable.
        assert_eq!(
            web("[web]\nport = 8790\naddress = \"192.168.1.42\"")
                .unwrap()
                .scannable_url()
                .as_deref(),
            Some("http://192.168.1.42:8790")
        );
        // The mesh range Tailscale hands out, which is the common case.
        assert_eq!(
            web("[web]\nport = 8790\naddress = \"100.101.102.103\"")
                .unwrap()
                .scannable_url()
                .as_deref(),
            Some("http://100.101.102.103:8790")
        );
        // v6 is bracketed, or it is not a URL.
        assert_eq!(
            web("[web]\nport = 8790\naddress = \"fd00::1\"")
                .unwrap()
                .scannable_url()
                .as_deref(),
            Some("http://[fd00::1]:8790")
        );

        // An explicit URL wins: a proxy terminating TLS elsewhere cannot be
        // worked out from what this process binds.
        assert_eq!(
            web("[web]\nport = 8795\naddress = \"100.101.102.103\"\nurl = \"https://host.tailnet.ts.net\"")
                .unwrap()
                .scannable_url()
                .as_deref(),
            Some("https://host.tailnet.ts.net")
        );

        // Nothing to scan when nothing else could reach it.
        assert_eq!(
            web("[web]\nport = 8790\naddress = \"127.0.0.1\"")
                .unwrap()
                .scannable_url(),
            None
        );
        assert_eq!(web("[web]\nport = 8790").unwrap().scannable_url(), None);
        assert_eq!(web("").unwrap().scannable_url(), None);
    }

    /// A browser client nobody serves cannot be reached, and a QR is an
    /// address rather than a code — so a configuration naming one without the
    /// other is a pairing screen that will disappoint somebody holding a phone.
    #[test]
    fn a_web_address_without_a_server_is_refused() {
        let with = |table: &str| {
            Config::parse(&format!(
                r#"
                {table}
                [[targets]]
                name = "development"
                ssh = "development-host"
            "#
            ))
        };

        let served = with("[web]\nport = 8790\nurl = \"https://host.tailnet.ts.net:8790\"")
            .expect("a served address is usable");
        assert_eq!(served.web.port, Some(8790));
        assert_eq!(
            served.web.url.as_deref(),
            Some("https://host.tailnet.ts.net:8790")
        );

        assert!(
            with("[web]\nurl = \"https://host.tailnet.ts.net:8790\"")
                .unwrap_err()
                .to_string()
                .contains("web.port does not serve")
        );
        // Refused where it is read rather than where it is served, so a file
        // that cannot work says so before somebody tries to pair a phone.
        assert!(with("[web]\nport = 8790\nurl = \"http://example.com\"").is_err());

        // Unset is a daemon that serves no browser client, which is the
        // default and must stay the default.
        let silent = with("").expect("no [web] table is fine");
        assert_eq!(silent.web, Default::default());
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

    #[test]
    fn replaces_and_removes_named_target_blocks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"# top-level comment
[transport]
command_timeout_seconds = 7

[[targets]]
name = "development"
ssh = "old-host"

[[targets]]
name = "build"
ssh = "build-host"
"#,
        )
        .unwrap();
        let replacement = Target {
            name: "development".to_owned(),
            ssh: Some("new-host".to_owned()),
            discover_sessions: true,
            session: None,
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
        };

        Config::replace_target_file(Some(&path), "development", replacement).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# top-level comment\n"));
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.transport.command_timeout_seconds, 7);
        assert_eq!(config.targets[0].endpoint(), "new-host");
        assert!(config.targets[0].discover_sessions);

        Config::remove_target_file(Some(&path), "build").unwrap();
        let config = Config::load(Some(&path)).unwrap().0;
        assert_eq!(config.targets.len(), 1);
        assert_eq!(config.targets[0].name, "development");
    }

    #[test]
    fn pairing_a_device_keeps_the_rest_of_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "# my hosts\n[[targets]]\nname = \"development\"\nssh = \"dev-host\"\n",
        )
        .unwrap();

        Config::add_device_file(
            Some(&path),
            Device {
                name: "phone".to_owned(),
                token_sha256: "aa".repeat(32),
                paired_at_ms: 1,
            },
        )
        .unwrap();
        Config::add_device_file(
            Some(&path),
            Device {
                name: "tablet".to_owned(),
                token_sha256: "bb".repeat(32),
                paired_at_ms: 2,
            },
        )
        .unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my hosts"), "comments survive");
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.targets.len(), 1);
        assert_eq!(
            config
                .devices
                .iter()
                .map(|device| device.name.as_str())
                .collect::<Vec<_>>(),
            vec!["phone", "tablet"]
        );

        // Pairing twice under one name is refused rather than silently
        // shadowing the first, which would leave a credential nobody can see.
        assert!(
            Config::add_device_file(
                Some(&path),
                Device {
                    name: "phone".to_owned(),
                    token_sha256: "cc".repeat(32),
                    paired_at_ms: 3,
                }
            )
            .is_err()
        );
    }

    #[test]
    fn revoking_a_device_removes_only_that_device() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "[[targets]]\nname = \"development\"\nssh = \"dev-host\"\n",
        )
        .unwrap();
        for (name, digest) in [("phone", "aa"), ("tablet", "bb"), ("laptop", "cc")] {
            Config::add_device_file(
                Some(&path),
                Device {
                    name: name.to_owned(),
                    token_sha256: digest.repeat(32),
                    paired_at_ms: 1,
                },
            )
            .unwrap();
        }

        Config::remove_device_file(Some(&path), "tablet").unwrap();

        let config = Config::parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            config
                .devices
                .iter()
                .map(|device| device.name.as_str())
                .collect::<Vec<_>>(),
            vec!["phone", "laptop"]
        );
        assert_eq!(config.targets.len(), 1, "targets are untouched");
        // Revoking what is not paired says so rather than reporting success.
        assert!(Config::remove_device_file(Some(&path), "tablet").is_err());
    }

    #[test]
    fn refuses_to_remove_the_final_target() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::add_target_file(
            Some(&path),
            Target {
                name: "only".to_owned(),
                ssh: None,
                discover_sessions: false,
                session: None,
                socket: None,
                herdr_bins: vec!["herdr".to_owned()],
            },
        )
        .unwrap();
        let before = fs::read(&path).unwrap();

        let error = Config::remove_target_file(Some(&path), "only").unwrap_err();

        assert!(error.to_string().contains("final configured target"));
        assert_eq!(fs::read(path).unwrap(), before);
    }
}
