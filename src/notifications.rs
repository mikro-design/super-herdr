use std::collections::VecDeque;
use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tokio::time::timeout;

use crate::attention::{AttentionEvent, AttentionEventKind};
use crate::config::NotificationsConfig;

const COALESCE_WINDOW: Duration = Duration::from_millis(500);
const MAX_PENDING_METADATA: usize = 16;

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

pub fn diagnostic_lines(config: &NotificationsConfig) -> Vec<String> {
    let delivery = native_backend()
        .map(|backend| backend.label().to_owned())
        .unwrap_or_else(|error| format!("unavailable ({error})"));
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

pub async fn deliver(delivery: NotificationDelivery) -> Result<()> {
    let mut command = match native_backend()? {
        #[cfg(target_os = "macos")]
        NativeBackend::MacOs => macos_command(&delivery),
        #[cfg(target_os = "linux")]
        NativeBackend::Freedesktop => freedesktop_command(&delivery),
    };
    let status = timeout(delivery.timeout, command.status())
        .await
        .context("native notification command timed out")?
        .context("failed to start the native notification command")?;
    if !status.success() {
        bail!("native notification command exited unsuccessfully");
    }
    Ok(())
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
fn freedesktop_command(delivery: &NotificationDelivery) -> Command {
    let mut command = Command::new("notify-send");
    command
        .arg("--app-name=Super-Herdr")
        .arg("--expire-time=10000")
        .arg(&delivery.title)
        .arg(&delivery.body);
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

    use super::{NotificationQueue, diagnostic_lines, notification_body, test_delivery};
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

    #[test]
    fn notification_diagnostics_and_tests_never_include_payloads() {
        let disabled = NotificationsConfig::default();
        assert!(test_delivery(&disabled).is_err());
        let lines = diagnostic_lines(&disabled).join("\n");
        assert!(lines.contains("notifications: disabled"));
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
