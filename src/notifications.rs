use std::collections::VecDeque;
use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::process::{Child, Command};
use tokio::time::timeout;

use crate::attention::{AttentionEvent, AttentionEventKind};
use crate::config::NotificationsConfig;
use crate::model::PaneId;

const COALESCE_WINDOW: Duration = Duration::from_millis(500);
const MAX_PENDING_METADATA: usize = 16;
/// How long a delivered notification stays on screen. A notification that can
/// report a click is waited on for this long plus the command timeout.
const EXPIRE: Duration = Duration::from_secs(10);
/// Identifier of the one offered action. The desktop reports it on stdout when
/// the notification is clicked.
const JUMP_ACTION: &str = "jump";

#[derive(Debug)]
pub struct NotificationQueue {
    config: NotificationsConfig,
    last_seen_id: Option<u64>,
    pending: VecDeque<AttentionEvent>,
    pending_count: usize,
    queued_at: Option<Instant>,
    last_attempt: Option<Instant>,
}

impl Default for NotificationQueue {
    fn default() -> Self {
        Self::new(NotificationsConfig::default(), None)
    }
}

impl NotificationQueue {
    pub fn new(config: NotificationsConfig, last_seen_id: Option<u64>) -> Self {
        Self {
            config,
            last_seen_id,
            pending: VecDeque::new(),
            pending_count: 0,
            queued_at: None,
            last_attempt: None,
        }
    }

    pub fn reconfigure(&mut self, config: NotificationsConfig) -> bool {
        if self.config == config {
            return false;
        }
        self.config = config;
        self.clear_pending();
        true
    }

    pub fn enqueue(&mut self, event: &AttentionEvent, now: Instant) -> bool {
        if self.last_seen_id.is_some_and(|id| event.id <= id) {
            return false;
        }
        self.last_seen_id = Some(event.id);
        if !self.config.enabled || !should_notify(&self.config, event.kind) {
            return false;
        }
        self.pending_count = self.pending_count.saturating_add(1);
        self.pending.push_back(event.clone());
        while self.pending.len() > MAX_PENDING_METADATA {
            self.pending.pop_front();
        }
        self.queued_at.get_or_insert(now);
        true
    }

    pub fn take_ready(&mut self, now: Instant) -> Option<NotificationDelivery> {
        if !self.config.enabled || self.pending_count == 0 {
            return None;
        }
        let queued_at = self.queued_at?;
        let coalesced_at = queued_at + COALESCE_WINDOW;
        let rate_limited_at = self
            .last_attempt
            .map(|attempt| attempt + Duration::from_secs(self.config.minimum_interval_seconds))
            .unwrap_or(queued_at);
        if now < coalesced_at.max(rate_limited_at) {
            return None;
        }

        let pending_count = self.pending_count;
        let latest = self.pending.back()?.clone();
        let delivery = NotificationDelivery {
            title: notification_title(pending_count, &latest),
            body: notification_body(pending_count, &latest),
            pane: Some(latest.pane.clone()),
            timeout: Duration::from_secs(self.config.command_timeout_seconds),
        };
        self.clear_pending();
        self.last_attempt = Some(now);
        Some(delivery)
    }

    fn clear_pending(&mut self) {
        self.pending.clear();
        self.pending_count = 0;
        self.queued_at = None;
    }
}

fn should_notify(config: &NotificationsConfig, kind: AttentionEventKind) -> bool {
    match kind {
        AttentionEventKind::NeedsAttention => config.needs_attention,
        AttentionEventKind::Working => config.working,
        AttentionEventKind::Completed => config.completed,
        AttentionEventKind::StatusChanged => config.status_changed,
        AttentionEventKind::Disappeared => config.disappeared,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationDelivery {
    title: String,
    body: String,
    /// The qualified pane a click should jump to. A synthetic test
    /// notification names no pane and therefore offers no action.
    pane: Option<PaneId>,
    timeout: Duration,
}

fn notification_title(count: usize, latest: &AttentionEvent) -> String {
    if count > 1 {
        return format!("{count} Super-Herdr agent updates");
    }
    match latest.kind {
        AttentionEventKind::NeedsAttention => "Agent needs attention".to_owned(),
        AttentionEventKind::Working => "Agent started working".to_owned(),
        AttentionEventKind::Completed => "Agent completed".to_owned(),
        AttentionEventKind::StatusChanged => "Agent status changed".to_owned(),
        AttentionEventKind::Disappeared => "Agent disappeared".to_owned(),
    }
}

fn notification_body(count: usize, latest: &AttentionEvent) -> String {
    let prefix = if count > 1 {
        format!("Latest of {count}: ")
    } else {
        String::new()
    };
    format!(
        "{prefix}{} · {}\n{}/{} · {}",
        bounded_metadata(&latest.agent, 128),
        bounded_metadata(&latest.workspace, 128),
        bounded_metadata(&latest.pane.target, 128),
        bounded_metadata(&latest.pane.session, 128),
        latest.kind.label()
    )
}

fn bounded_metadata(value: &str, maximum_characters: usize) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(maximum_characters)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeBackend {
    #[cfg(target_os = "macos")]
    MacOs,
    #[cfg(target_os = "linux")]
    Freedesktop,
}

impl NativeBackend {
    fn label(self) -> &'static str {
        match self {
            #[cfg(target_os = "macos")]
            Self::MacOs => "macOS Notification Center via osascript",
            #[cfg(target_os = "linux")]
            Self::Freedesktop => "desktop notifications via notify-send",
        }
    }
}

pub async fn diagnostic_lines(config: &NotificationsConfig) -> Vec<String> {
    let backend = native_backend();
    let delivery = backend
        .as_ref()
        .map(|backend| backend.label().to_owned())
        .unwrap_or_else(|error| format!("unavailable ({error})"));
    let click = match backend {
        Ok(backend) if native_capabilities(backend).await.actions => {
            "available; clicking a notification jumps to its qualified pane".to_owned()
        }
        Ok(_) => "unavailable (this desktop's notification tool cannot report a click)".to_owned(),
        Err(_) => "unavailable".to_owned(),
    };
    let mut enabled_kinds = Vec::new();
    if config.needs_attention {
        enabled_kinds.push("needs_attention");
    }
    if config.completed {
        enabled_kinds.push("completed");
    }
    if config.disappeared {
        enabled_kinds.push("disappeared");
    }
    if config.working {
        enabled_kinds.push("working");
    }
    if config.status_changed {
        enabled_kinds.push("status_changed");
    }
    vec![
        format!(
            "notifications: {}",
            if config.enabled {
                "enabled"
            } else {
                "disabled"
            }
        ),
        format!("delivery: {delivery}"),
        format!("click to jump: {click}"),
        format!("events: {}", enabled_kinds.join(", ")),
        format!(
            "minimum interval: {} second(s)",
            config.minimum_interval_seconds
        ),
        "notifications contain metadata only; terminal and clipboard contents are excluded"
            .to_owned(),
    ]
}

pub fn test_delivery(config: &NotificationsConfig) -> Result<NotificationDelivery> {
    if !config.enabled {
        bail!("notifications are disabled in configuration");
    }
    Ok(NotificationDelivery {
        title: "Super-Herdr notification test".to_owned(),
        body: "Native metadata-only notification delivery is working".to_owned(),
        pane: None,
        timeout: Duration::from_secs(config.command_timeout_seconds),
    })
}

fn native_backend() -> Result<NativeBackend> {
    if env::var_os("HERDR_ENV").as_deref() == Some(OsStr::new("1"))
        || env::var_os("SSH_CONNECTION").is_some()
        || env::var_os("SSH_TTY").is_some()
    {
        bail!("native notifications require Super-Herdr to run on the desktop");
    }

    #[cfg(target_os = "macos")]
    if command_available("osascript") {
        return Ok(NativeBackend::MacOs);
    }

    #[cfg(target_os = "linux")]
    if (env::var_os("WAYLAND_DISPLAY").is_some() || env::var_os("DISPLAY").is_some())
        && command_available("notify-send")
    {
        return Ok(NativeBackend::Freedesktop);
    }

    #[cfg(target_os = "macos")]
    bail!("native notifications require osascript");
    #[cfg(target_os = "linux")]
    bail!("native notifications require a desktop session and notify-send");
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    bail!("native notifications are unsupported on this platform");
}

/// What a desktop can report back about a notification it displayed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeCapabilities {
    /// The notification tool can offer a clickable action and report it.
    pub actions: bool,
}

/// A notification the desktop is showing. Delivery already succeeded; the
/// child is retained only to learn whether the notification is clicked.
#[derive(Debug)]
pub struct PendingNotification {
    child: Child,
    pane: Option<PaneId>,
    activation: bool,
    timeout: Duration,
}

/// Show a notification and return once the desktop has accepted it. The click
/// itself is awaited separately so a burst is never gated on a person.
pub async fn dispatch(delivery: &NotificationDelivery) -> Result<PendingNotification> {
    let backend = native_backend()?;
    let capabilities = native_capabilities(backend).await;
    let activation = capabilities.actions && delivery.pane.is_some();
    let mut command = match backend {
        #[cfg(target_os = "macos")]
        NativeBackend::MacOs => macos_command(delivery),
        #[cfg(target_os = "linux")]
        NativeBackend::Freedesktop => freedesktop_command(delivery, activation),
    };
    if activation {
        command.stdout(Stdio::piped());
    }
    let child = command
        .spawn()
        .context("failed to start the native notification command")?;
    Ok(PendingNotification {
        child,
        pane: delivery.pane.clone(),
        activation,
        timeout: delivery.timeout,
    })
}

/// Wait for the notification to close and report the qualified pane when the
/// person clicked its action. The wait is bounded by how long the notification
/// can stay on screen, so an ignored notification always releases its child.
pub async fn wait_for_activation(pending: PendingNotification) -> Result<Option<PaneId>> {
    let PendingNotification {
        mut child,
        pane,
        activation,
        timeout: command_timeout,
    } = pending;
    if !activation {
        let status = timeout(command_timeout, child.wait())
            .await
            .context("native notification command timed out")?
            .context("failed to run the native notification command")?;
        if !status.success() {
            bail!("native notification command exited unsuccessfully");
        }
        return Ok(None);
    }

    let output = timeout(EXPIRE + command_timeout, child.wait_with_output())
        .await
        .context("native notification stayed open longer than its expiry")?
        .context("failed to read the native notification result")?;
    if !output.status.success() {
        bail!("native notification command exited unsuccessfully");
    }
    Ok(activated_pane(
        &String::from_utf8_lossy(&output.stdout),
        pane,
    ))
}

/// Show a notification and wait only for the desktop to accept it, ignoring any
/// click. This is the one-shot path used by `notifications test`.
pub async fn deliver(delivery: NotificationDelivery) -> Result<()> {
    let mut pending = dispatch(&delivery).await?;
    pending.activation = false;
    wait_for_activation(pending).await.map(|_| ())
}

/// The desktop prints the identifier of the invoked action. Anything else—an
/// expiry, a dismissal, an unrelated line—is not a jump.
fn activated_pane(stdout: &str, pane: Option<PaneId>) -> Option<PaneId> {
    stdout
        .lines()
        .any(|line| line.trim() == JUMP_ACTION)
        .then_some(pane)
        .flatten()
}

static NATIVE_CAPABILITIES: OnceLock<NativeCapabilities> = OnceLock::new();

/// Probe the notification tool once per process. An older `notify-send` rejects
/// unknown options outright, so the flags are used only when it advertises
/// them.
async fn native_capabilities(backend: NativeBackend) -> NativeCapabilities {
    if let Some(cached) = NATIVE_CAPABILITIES.get() {
        return *cached;
    }
    let probed = match backend {
        // `osascript` displays a notification but cannot report a click, so
        // macOS delivery stays one-way.
        #[cfg(target_os = "macos")]
        NativeBackend::MacOs => NativeCapabilities::default(),
        #[cfg(target_os = "linux")]
        NativeBackend::Freedesktop => {
            let mut command = Command::new("notify-send");
            command.arg("--help");
            isolate_command(&mut command);
            let help = timeout(Duration::from_secs(2), command.output()).await;
            match help {
                Ok(Ok(output)) => parse_notify_send_capabilities(&format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )),
                _ => NativeCapabilities::default(),
            }
        }
    };
    *NATIVE_CAPABILITIES.get_or_init(|| probed)
}

/// Click reporting needs both an action to offer and the option to wait for it;
/// libnotify gained them in 0.8.
#[cfg(target_os = "linux")]
fn parse_notify_send_capabilities(help: &str) -> NativeCapabilities {
    NativeCapabilities {
        actions: help.contains("--action") && help.contains("--wait"),
    }
}

#[cfg(target_os = "macos")]
fn macos_command(delivery: &NotificationDelivery) -> Command {
    let script = r#"display notification (system attribute "SUPER_HERDR_NOTIFICATION_BODY") with title (system attribute "SUPER_HERDR_NOTIFICATION_TITLE")"#;
    let mut command = Command::new("osascript");
    command
        .arg("-e")
        .arg(script)
        .env("SUPER_HERDR_NOTIFICATION_TITLE", &delivery.title)
        .env("SUPER_HERDR_NOTIFICATION_BODY", &delivery.body);
    isolate_command(&mut command);
    command
}

#[cfg(target_os = "linux")]
fn freedesktop_command(delivery: &NotificationDelivery, activation: bool) -> Command {
    let mut command = Command::new("notify-send");
    command
        .arg("--app-name=Super-Herdr")
        .arg(format!("--expire-time={}", EXPIRE.as_millis()));
    if activation {
        command
            .arg("--wait")
            .arg(format!("--action={JUMP_ACTION}=Jump to pane"));
    }
    command.arg(&delivery.title).arg(&delivery.body);
    isolate_command(&mut command);
    command
}

fn isolate_command(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
}

fn command_available(program: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|directory| executable(directory.join(program).as_path()))
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        NotificationQueue, activated_pane, diagnostic_lines, notification_body, test_delivery,
    };
    use crate::attention::{AttentionEvent, AttentionEventKind};
    use crate::config::NotificationsConfig;
    use crate::model::PaneId;

    fn event(id: u64, kind: AttentionEventKind) -> AttentionEvent {
        AttentionEvent {
            id,
            pane: PaneId::new("host-a", "work", "w1:p1"),
            agent: "builder".to_owned(),
            workspace: "compiler".to_owned(),
            status: "payload-free metadata".to_owned(),
            kind,
            occurred_at_ms: 1,
            unread: true,
        }
    }

    #[test]
    fn notifications_are_opt_in_and_skip_historical_or_filtered_events() {
        let start = Instant::now();
        let mut queue = NotificationQueue::new(NotificationsConfig::default(), Some(3));
        assert!(!queue.enqueue(&event(3, AttentionEventKind::NeedsAttention), start));
        assert!(!queue.enqueue(&event(4, AttentionEventKind::NeedsAttention), start));

        let config = NotificationsConfig {
            enabled: true,
            ..NotificationsConfig::default()
        };
        assert!(queue.reconfigure(config));
        assert!(!queue.enqueue(&event(5, AttentionEventKind::Working), start));
        assert!(queue.enqueue(&event(6, AttentionEventKind::NeedsAttention), start));
        assert!(
            queue
                .take_ready(start + Duration::from_millis(499))
                .is_none()
        );
        let delivery = queue
            .take_ready(start + Duration::from_millis(500))
            .unwrap();
        assert_eq!(delivery.title, "Agent needs attention");
        assert!(delivery.body.contains("builder · compiler"));
        assert!(delivery.body.contains("host-a/work · needs input"));
        assert!(!delivery.body.contains("payload-free metadata"));
    }

    #[test]
    fn notifications_coalesce_and_rate_limit_batches() {
        let start = Instant::now();
        let config = NotificationsConfig {
            enabled: true,
            minimum_interval_seconds: 5,
            ..NotificationsConfig::default()
        };
        let mut queue = NotificationQueue::new(config, None);
        assert!(queue.enqueue(&event(1, AttentionEventKind::Completed), start));
        let first = queue
            .take_ready(start + Duration::from_millis(500))
            .unwrap();
        assert_eq!(first.title, "Agent completed");

        assert!(queue.enqueue(
            &event(2, AttentionEventKind::NeedsAttention),
            start + Duration::from_secs(1)
        ));
        assert!(queue.enqueue(
            &event(3, AttentionEventKind::Completed),
            start + Duration::from_secs(1)
        ));
        assert!(queue.take_ready(start + Duration::from_secs(4)).is_none());
        let second = queue.take_ready(start + Duration::from_secs(6)).unwrap();
        assert_eq!(second.title, "2 Super-Herdr agent updates");
        assert!(
            notification_body(2, &event(3, AttentionEventKind::Completed))
                .starts_with("Latest of 2:")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn click_reporting_needs_both_an_action_and_a_wait() {
        use super::parse_notify_send_capabilities;

        // libnotify 0.7.9, still shipped by current LTS distributions.
        let old_help = "  -u, --urgency=LEVEL\n  -t, --expire-time=TIME\n  -h, --hint=TYPE\n";
        assert!(!parse_notify_send_capabilities(old_help).actions);

        let new_help = "  -A, --action=[NAME=]Text\n  -w, --wait\n  -t, --expire-time=TIME\n";
        assert!(parse_notify_send_capabilities(new_help).actions);

        // A tool that can offer an action but never reports it is not enough.
        assert!(!parse_notify_send_capabilities("  -A, --action=NAME\n").actions);
    }

    #[test]
    fn an_activation_is_only_the_offered_action() {
        let pane = PaneId::new("build", "toolchains", "w2:p1");
        assert_eq!(
            activated_pane("jump\n", Some(pane.clone())),
            Some(pane.clone())
        );
        assert_eq!(activated_pane("", Some(pane.clone())), None);
        assert_eq!(activated_pane("closed\n", Some(pane.clone())), None);
        // A notification that names no pane can never move the selection.
        assert_eq!(activated_pane("jump\n", None), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_clickable_notification_offers_one_action_and_no_payload() {
        use super::{NotificationDelivery, freedesktop_command};

        let delivery = NotificationDelivery {
            title: "Agent needs attention".to_owned(),
            body: "codex · simulator\nbuild/toolchains · needs input".to_owned(),
            pane: Some(PaneId::new("build", "toolchains", "w2:p1")),
            timeout: Duration::from_secs(5),
        };

        let arguments = |activation| {
            freedesktop_command(&delivery, activation)
                .as_std()
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };

        let clickable = arguments(true);
        assert!(clickable.contains(&"--wait".to_owned()));
        assert!(clickable.contains(&"--action=jump=Jump to pane".to_owned()));
        assert!(clickable.contains(&"--expire-time=10000".to_owned()));
        // The pane identifier routes the click; it is never written into the
        // notification text.
        assert!(!clickable.iter().any(|argument| argument.contains("w2:p1")));

        let plain = arguments(false);
        assert!(
            !plain
                .iter()
                .any(|argument| argument.starts_with("--action"))
        );
        assert!(!plain.contains(&"--wait".to_owned()));
        assert_eq!(plain.last(), clickable.last());
    }

    #[tokio::test]
    async fn notification_diagnostics_and_tests_never_include_payloads() {
        let disabled = NotificationsConfig::default();
        assert!(test_delivery(&disabled).is_err());
        let lines = diagnostic_lines(&disabled).await.join("\n");
        assert!(lines.contains("notifications: disabled"));
        assert!(lines.contains("click to jump:"));
        assert!(lines.contains("terminal and clipboard contents are excluded"));

        let enabled = NotificationsConfig {
            enabled: true,
            ..NotificationsConfig::default()
        };
        let delivery = test_delivery(&enabled).unwrap();
        assert!(delivery.title.contains("notification test"));
        assert!(!delivery.body.contains("terminal"));
        assert!(!delivery.body.contains("clipboard"));
    }
}
