use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Read, Stdout, Write};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Tabs,
};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;
use tokio::time::{interval, sleep_until};

use crate::attention::{AttentionEvent, AttentionEventKind, AttentionIndex};
use crate::client::{Client, ClientCommands};
use crate::clipboard;
use crate::config::{Config, Target, TransportConfig};
use crate::daemon::server::{DaemonOptions, spawn_in_process};
use crate::model::{PaneId, TabId, TargetSession, WorkspaceId};
use crate::notifications::{self, NotificationDelivery, NotificationQueue};
use crate::operation::Operation;
use crate::protocol::ServerMessage;
use crate::resource_action::{ResourceAction, SplitDirection};
use crate::state::{
    FederationState, NormalizedSnapshot, TargetConnectionState, TargetRuntimeState,
    TargetUpdateMode,
};
use crate::terminal::{TerminalAccess, TerminalScrollDirection};
use crate::transport::{
    DiscoveredSession, SessionDiscovery, discover_running_sessions, expand_discovered_sessions,
};
use crate::ui_state::{UiState, UiStateStore};

const PREFIX_KEY: u8 = 0x1d;
const HERDR_PREFIX_KEY: u8 = 0x02;
const SIDEBAR_WIDTH: u16 = 28;
const INPUT_ESCAPE_TIMEOUT: Duration = Duration::from_millis(30);
const CLIPBOARD_FEEDBACK_DURATION: Duration = Duration::from_secs(3);
const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
const MAX_CLIPBOARD_MEDIA_BYTES: usize = 32 * 1024 * 1024;
const MOUSE_SCROLL_LINES: u16 = 3;
const MOUSE_CAPTURE_ENABLE: &[u8] = b"\x1b[?1002h\x1b[?1006h";
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";
const MIN_RENDER_INTERVAL: Duration = Duration::from_millis(16);
const ROUTE_EVENT_DRAIN_LIMIT: usize = 64;
const SELECTION_AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(30);
const CONFIG_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Terminal,
    Prefix,
    HerdrPrefix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MouseInput {
    code: u16,
    column: u16,
    row: u16,
    release: bool,
}

impl MouseInput {
    fn is_left_press(self) -> bool {
        !self.release && self.code & 0b0110_0011 == 0
    }

    fn is_left_motion(self) -> bool {
        !self.release && self.code & 0b0110_0011 == 0b0010_0000
    }

    fn is_left_release(self) -> bool {
        self.release && self.code & 0b0110_0011 == 0
    }

    fn is_right_press(self) -> bool {
        !self.release && self.code & 0b0110_0011 == 2
    }

    fn is_motion(self) -> bool {
        self.code & 0b0010_0000 != 0
    }

    fn is_vertical_wheel(self) -> bool {
        !self.release && self.code & 0b0100_0000 != 0 && self.code & 0b11 <= 1
    }

    fn scroll_direction(self) -> Option<TerminalScrollDirection> {
        if !self.is_vertical_wheel() {
            return None;
        }
        match self.code & 0b11 {
            0 => Some(TerminalScrollDirection::Up),
            1 => Some(TerminalScrollDirection::Down),
            _ => None,
        }
    }

    fn key_modifiers(self) -> u8 {
        let mut modifiers = crossterm::event::KeyModifiers::empty();
        if self.code & 0b0000_0100 != 0 {
            modifiers.insert(crossterm::event::KeyModifiers::SHIFT);
        }
        if self.code & 0b0000_1000 != 0 {
            modifiers.insert(crossterm::event::KeyModifiers::ALT);
        }
        if self.code & 0b0001_0000 != 0 {
            modifiers.insert(crossterm::event::KeyModifiers::CONTROL);
        }
        modifiers.bits()
    }

    fn shift(self) -> bool {
        self.code & 0b0000_0100 != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DecodedInput {
    Bytes(Vec<u8>),
    Mouse(MouseInput),
    Paste(Vec<u8>),
    PasteTooLarge,
}

#[derive(Default)]
struct InputDecoder {
    pending: Vec<u8>,
    pending_since: Option<Instant>,
    paste: Option<PasteBuffer>,
}

#[derive(Default)]
struct PasteBuffer {
    bytes: Vec<u8>,
    overflow_tail: Vec<u8>,
    overflowed: bool,
}

impl InputDecoder {
    fn push(&mut self, byte: u8) -> Option<DecodedInput> {
        if let Some(paste) = self.paste.as_mut() {
            if paste.overflowed {
                paste.overflow_tail.push(byte);
                if paste.overflow_tail.len() > BRACKETED_PASTE_END.len() {
                    paste.overflow_tail.remove(0);
                }
                if paste.overflow_tail == BRACKETED_PASTE_END {
                    self.paste = None;
                    return Some(DecodedInput::PasteTooLarge);
                }
                return None;
            }

            paste.bytes.push(byte);
            if paste.bytes.ends_with(BRACKETED_PASTE_END) {
                paste
                    .bytes
                    .truncate(paste.bytes.len() - BRACKETED_PASTE_END.len());
                let paste = self.paste.take().expect("paste buffer was present");
                return Some(if paste.bytes.len() <= MAX_CLIPBOARD_BYTES {
                    DecodedInput::Paste(paste.bytes)
                } else {
                    DecodedInput::PasteTooLarge
                });
            }
            if paste.bytes.len() > MAX_CLIPBOARD_BYTES + BRACKETED_PASTE_END.len() {
                paste.overflow_tail = paste.bytes
                    [paste.bytes.len().saturating_sub(BRACKETED_PASTE_END.len())..]
                    .to_vec();
                paste.bytes.clear();
                paste.overflowed = true;
            }
            return None;
        }

        if self.pending.is_empty() {
            if byte == 0x1b {
                self.pending.push(byte);
                self.pending_since = Some(Instant::now());
                return None;
            }
            return Some(DecodedInput::Bytes(vec![byte]));
        }

        self.pending.push(byte);
        if BRACKETED_PASTE_START.starts_with(&self.pending) {
            if self.pending == BRACKETED_PASTE_START {
                self.pending.clear();
                self.pending_since = None;
                self.paste = Some(PasteBuffer::default());
            }
            return None;
        }
        let is_mouse_prefix = match self.pending.as_slice() {
            [0x1b] | [0x1b, b'['] => true,
            bytes if bytes.starts_with(b"\x1b[<") => true,
            _ => false,
        };
        if !is_mouse_prefix || self.pending.len() > 64 {
            return Some(self.flush_bytes());
        }

        if matches!(byte, b'M' | b'm') {
            let bytes = std::mem::take(&mut self.pending);
            self.pending_since = None;
            return Some(match parse_sgr_mouse(&bytes) {
                Some(mouse) => DecodedInput::Mouse(mouse),
                None => DecodedInput::Bytes(bytes),
            });
        }
        if !matches!(byte, b'0'..=b'9' | b';' | b'<' | b'[' | 0x1b) {
            return Some(self.flush_bytes());
        }
        None
    }

    fn flush_expired(&mut self) -> Option<DecodedInput> {
        self.pending_since
            .is_some_and(|started| started.elapsed() >= INPUT_ESCAPE_TIMEOUT)
            .then(|| self.flush_bytes())
    }

    fn flush_bytes(&mut self) -> DecodedInput {
        self.pending_since = None;
        DecodedInput::Bytes(std::mem::take(&mut self.pending))
    }
}

fn parse_sgr_mouse(bytes: &[u8]) -> Option<MouseInput> {
    let body = bytes.strip_prefix(b"\x1b[<")?;
    let (&terminator, body) = body.split_last()?;
    if !matches!(terminator, b'M' | b'm') {
        return None;
    }
    let body = std::str::from_utf8(body).ok()?;
    let mut fields = body.split(';');
    let code = fields.next()?.parse().ok()?;
    let column = fields.next()?.parse().ok()?;
    let row = fields.next()?.parse().ok()?;
    if fields.next().is_some() || column == 0 || row == 0 {
        return None;
    }
    Some(MouseInput {
        code,
        column,
        row,
        release: terminator == b'm',
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CellPosition {
    row: i64,
    column: u16,
}

impl CellPosition {
    fn from_viewport(row: u16, column: u16, viewport_offset: i64) -> Self {
        Self {
            row: i64::from(row) - viewport_offset,
            column,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CapturedCell {
    Text(String),
    WideContinuation,
}

type CapturedRow = Vec<CapturedCell>;

#[derive(Debug, Clone)]
struct TerminalSelection {
    pane: PaneId,
    anchor: CellPosition,
    head: CellPosition,
    viewport_offset: i64,
    captured_rows: BTreeMap<i64, CapturedRow>,
    dragging: bool,
    forwarded_click: Option<MouseInput>,
}

impl TerminalSelection {
    fn contains(&self, position: CellPosition) -> bool {
        let start = self.anchor.min(self.head);
        let end = self.anchor.max(self.head);
        position >= start && position <= end
    }

    fn finish(
        &mut self,
        position: CellPosition,
        viewport_position: CellPosition,
        release: MouseInput,
    ) -> SelectionFinish {
        self.head = position;
        self.dragging |= self.head != self.anchor;
        if self.dragging {
            self.forwarded_click = None;
            SelectionFinish::Retain
        } else if let Some(press) = self.forwarded_click.take() {
            SelectionFinish::ForwardClick([
                press,
                MouseInput {
                    column: position.column + 1,
                    row: u16::try_from(viewport_position.row)
                        .unwrap_or_default()
                        .saturating_add(1),
                    ..release
                },
            ])
        } else {
            SelectionFinish::Clear
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionFinish {
    Retain,
    ForwardClick([MouseInput; 2]),
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionAutoscrollDirection {
    Up,
    Down,
}

#[derive(Debug, Clone)]
struct SelectionAutoscroll {
    pane: PaneId,
    direction: SelectionAutoscrollDirection,
    column: u16,
    pending_lines: usize,
}

struct ClipboardFeedback {
    text: String,
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetFormField {
    Name,
    Ssh,
    Session,
    DiscoverSessions,
    DiscoveredSession,
}

impl TargetFormField {
    fn next(self, has_discovered_sessions: bool) -> Self {
        match self {
            Self::Name => Self::Ssh,
            Self::Ssh => Self::Session,
            Self::Session => Self::DiscoverSessions,
            Self::DiscoverSessions if has_discovered_sessions => Self::DiscoveredSession,
            Self::DiscoverSessions | Self::DiscoveredSession => Self::Name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetTestState {
    Untested,
    Testing { request_id: u64 },
    Passed { summary: String },
    Failed { message: String },
}

#[derive(Debug, Clone)]
struct TargetForm {
    original_name: Option<String>,
    name: String,
    ssh: String,
    session: String,
    discover_sessions: bool,
    socket: Option<String>,
    herdr_bins: Vec<String>,
    field: TargetFormField,
    error: Option<String>,
    test_state: TargetTestState,
    discovered_sessions: Vec<DiscoveredSession>,
    discovered_selected: usize,
    discovered_offset: usize,
}

impl TargetForm {
    fn add() -> Self {
        Self {
            original_name: None,
            name: String::new(),
            ssh: String::new(),
            session: String::new(),
            discover_sessions: true,
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
            field: TargetFormField::Name,
            error: None,
            test_state: TargetTestState::Untested,
            discovered_sessions: Vec::new(),
            discovered_selected: 0,
            discovered_offset: 0,
        }
    }

    fn edit(target: &Target) -> Self {
        Self {
            original_name: Some(target.name.clone()),
            name: target.name.clone(),
            ssh: target.ssh.clone().unwrap_or_default(),
            session: target.session.clone().unwrap_or_default(),
            discover_sessions: target.discover_sessions,
            socket: target.socket.clone(),
            herdr_bins: target.herdr_bins.clone(),
            field: TargetFormField::Name,
            error: None,
            test_state: TargetTestState::Untested,
            discovered_sessions: Vec::new(),
            discovered_selected: 0,
            discovered_offset: 0,
        }
    }

    fn target(&self) -> Target {
        Target {
            name: self.name.clone(),
            ssh: (!self.ssh.is_empty()).then(|| self.ssh.clone()),
            discover_sessions: self.discover_sessions,
            session: (!self.session.is_empty()).then(|| self.session.clone()),
            socket: self.socket.clone(),
            herdr_bins: self.herdr_bins.clone(),
        }
    }

    fn active_text_mut(&mut self) -> Option<&mut String> {
        match self.field {
            TargetFormField::Name => Some(&mut self.name),
            TargetFormField::Ssh => Some(&mut self.ssh),
            TargetFormField::Session => Some(&mut self.session),
            TargetFormField::DiscoverSessions | TargetFormField::DiscoveredSession => None,
        }
    }

    fn validation_error(&self, configured: &[Target]) -> Option<String> {
        let target = self.target();
        let mut targets = configured.to_vec();
        match self.original_name.as_deref() {
            Some(original_name) => {
                let Some(index) = targets
                    .iter()
                    .position(|candidate| candidate.name == original_name)
                else {
                    return Some(format!("target {original_name:?} is no longer configured"));
                };
                targets[index] = target;
            }
            None => targets.push(target),
        }
        Config {
            transport: TransportConfig::default(),
            notifications: crate::config::NotificationsConfig::default(),
            transfers: Default::default(),
            targets,
            devices: Vec::new(),
        }
        .validate()
        .err()
        .map(|error| error.to_string())
    }

    fn invalidate_test(&mut self) {
        self.error = None;
        self.test_state = TargetTestState::Untested;
        self.discovered_sessions.clear();
        self.discovered_selected = 0;
        self.discovered_offset = 0;
        if self.field == TargetFormField::DiscoveredSession {
            self.field = TargetFormField::Session;
        }
    }

    fn select_discovered_session(&mut self, index: usize) {
        let Some(session) = self.discovered_sessions.get(index) else {
            return;
        };
        self.session.clone_from(&session.name);
        self.socket = Some(session.socket_path.clone());
        self.discover_sessions = false;
        self.discovered_selected = index;
        self.field = TargetFormField::DiscoveredSession;
        self.error = None;
    }
}

#[derive(Debug, Clone)]
enum TargetManagerMode {
    List,
    Form(Box<TargetForm>),
    ConfirmRemove { name: String },
}

#[derive(Debug, Clone)]
struct TargetManager {
    targets: Vec<Target>,
    selected: usize,
    mode: TargetManagerMode,
    mouse_press: Option<TargetManagerControl>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetManagerControl {
    Target(usize),
    Add,
    Edit,
    Remove,
    Close,
    Field(TargetFormField),
    TestConnection,
    Save,
    Cancel,
    DiscoveredSession(usize),
    ConfirmRemove,
    CancelRemove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentFilter {
    All,
    Attention,
    Active,
}

impl AgentFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Attention,
            Self::Attention => Self::Active,
            Self::Active => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Attention => "attention",
            Self::Active => "active",
        }
    }
}

#[derive(Debug, Clone)]
struct AgentNavigator {
    filter: AgentFilter,
    selected: usize,
}

impl Default for AgentNavigator {
    fn default() -> Self {
        Self {
            filter: AgentFilter::Attention,
            selected: 0,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct AttentionCenter {
    selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentJumpEntry {
    pane: PaneId,
    agent: String,
    workspace: String,
    status: String,
    interactive_ready: bool,
}

impl TargetManager {
    fn new(targets: Vec<Target>) -> Self {
        Self {
            targets,
            selected: 0,
            mode: TargetManagerMode::List,
            mouse_press: None,
        }
    }

    fn selected_target(&self) -> Option<&Target> {
        self.targets.get(self.selected)
    }
}

#[derive(Debug, Clone)]
struct SidebarHitArea {
    area: Rect,
    pane: Option<PaneId>,
    context: Option<ContextTarget>,
}

struct SidebarRow {
    line: Line<'static>,
    pane: Option<PaneId>,
    context: Option<ContextTarget>,
    selection_anchor: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ContextTarget {
    Session(TargetSession),
    Workspace(WorkspaceId),
    Tab(TabId),
    Pane(PaneId),
}

/// A pane this frontend is watching.
///
/// The terminal itself lives with the daemon; what is local is the screen model
/// and the two facts that describe the subscription. `requested` is what was
/// asked for and is compared against what the layout now wants, while `access`
/// is what the daemon actually granted. Keeping them apart is what stops a
/// downgraded lease from looking like a stale subscription and being resubscribed
/// forever.
struct ActiveRoute {
    pane: PaneId,
    requested: TerminalAccess,
    access: TerminalAccess,
    generation: u64,
    rows: u16,
    columns: u16,
    parser: vt100::Parser,
    last_sequence: Option<u64>,
}

/// What a frontend needs to remember about an operation while the daemon runs
/// it, since the result comes back carrying only a request identifier.
struct PendingOperation {
    description: String,
    follow_server_focus: bool,
    /// Reported through the transient feedback line instead of the message
    /// line, which is how clipboard results have always been shown.
    clipboard_feedback: Option<String>,
}

/// Targets by the qualified key they are addressed with, after discovery.
fn resolved_targets(config: &Config) -> BTreeMap<TargetSession, Target> {
    config
        .targets
        .iter()
        .map(|target| (crate::state::target_key(target), target.clone()))
        .collect()
}

/// Whether a target can take an atomic paste, which needs a documented Herdr
/// API socket rather than a terminal stream.
///
/// Asked of the resolved targets rather than the configured ones, because those
/// are different things whenever discovery is doing its job: a target with
/// `discover_sessions` and no `socket` line has no socket in the file and a
/// perfectly good one at runtime. Asking the file returns no for a target that
/// can, which is a paste refused for a reason that is not true.
fn atomic_paste_available(resolved: &BTreeMap<TargetSession, Target>, key: &TargetSession) -> bool {
    resolved
        .get(key)
        .is_some_and(|target| target.socket.is_some())
}

/// What the frontend needs to remember while the daemon moves a payload. The
/// path comes back verified, and pasting it is this side's job because
/// bracketed-paste state lives with the parser.
struct PendingUpload {
    mime: String,
}

enum ConfigRefresh {
    /// Boxed because a refresh carries two whole configurations and a failure
    /// carries a sentence, and every message of this type would otherwise be
    /// sized for the larger.
    Ready(Box<ConfigRefreshed>),
    Failed(String),
}

struct ConfigRefreshed {
    configured: Config,
    expanded: Config,
}

struct TargetTestEvent {
    request_id: u64,
    elapsed_ms: u128,
    result: Result<SessionDiscovery, String>,
}

/// What a native notification reported back: whether the desktop accepted it,
/// and later whether the person clicked it.
enum NotificationEvent {
    Delivered(Result<(), String>),
    Activated(PaneId),
}

#[derive(Debug, Clone, Default)]
struct CommandPalette {
    query: String,
    selected: usize,
}

#[derive(Debug, Clone)]
struct ContextMenu {
    title: String,
    actions: Vec<ResourceAction>,
    selected: usize,
    anchor: Position,
    pressed: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TextPromptAction {
    CreateWorkspace { target: TargetSession },
    RenameWorkspace { workspace: WorkspaceId },
    RenameTab { tab: TabId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TextPrompt {
    title: String,
    label: String,
    value: String,
    action: TextPromptAction,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CloseConfirmation {
    action: ResourceAction,
}

struct App {
    selected_pane: Option<PaneId>,
    selection_explicit: bool,
    restore_pending: Option<PaneId>,
    state_store: Option<UiStateStore>,
    mode: InputMode,
    routes: BTreeMap<PaneId, ActiveRoute>,
    route_retry_after: BTreeMap<PaneId, Instant>,
    /// Commands to the daemon. Held here rather than threaded through every
    /// function that might type into a pane, matching how the frontend already
    /// carries its other senders.
    client: Option<ClientCommands>,
    pending_operations: BTreeMap<u64, PendingOperation>,
    pending_uploads: BTreeMap<u64, PendingUpload>,
    last_frame_area: Option<Rect>,
    last_terminal_area: Option<Rect>,
    selection: Option<TerminalSelection>,
    selection_autoscroll: Option<SelectionAutoscroll>,
    swallow_left_gesture: bool,
    sidebar_offset: usize,
    attention_sidebar_offset: usize,
    sidebar_follow_selected: bool,
    sidebar_last_selected: Option<PaneId>,
    sidebar_hit_areas: Vec<SidebarHitArea>,
    sidebar_press: Option<PaneId>,
    last_render_at: Option<Instant>,
    message: Option<String>,
    clipboard_feedback: Option<ClipboardFeedback>,
    config_path: Option<PathBuf>,
    configured_targets: Vec<Target>,
    /// Targets as they resolved, which is not what the file says. A session
    /// discovered at runtime carries the socket the host reported, and a target
    /// that relies on discovery has none in the file at all — so a question
    /// about what a target can do has to be asked of this rather than of the
    /// configuration the frontend happens to hold.
    resolved_targets: BTreeMap<TargetSession, Target>,
    configuration_dirty: bool,
    target_manager: Option<TargetManager>,
    target_test_sender: Option<mpsc::UnboundedSender<TargetTestEvent>>,
    next_target_test_request: u64,
    agent_navigator: Option<AgentNavigator>,
    attention: AttentionIndex,
    attention_center: Option<AttentionCenter>,
    notification_queue: NotificationQueue,
    notification_sender: Option<mpsc::UnboundedSender<NotificationEvent>>,
    notification_inflight: bool,
    notification_error_reported: bool,
    command_palette: Option<CommandPalette>,
    context_menu: Option<ContextMenu>,
    text_prompt: Option<TextPrompt>,
    close_confirmation: Option<CloseConfirmation>,
    herdr_action_inflight: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            selected_pane: None,
            selection_explicit: false,
            restore_pending: None,
            state_store: None,
            mode: InputMode::Terminal,
            routes: BTreeMap::new(),
            route_retry_after: BTreeMap::new(),
            client: None,
            pending_operations: BTreeMap::new(),
            pending_uploads: BTreeMap::new(),
            last_frame_area: None,
            last_terminal_area: None,
            selection: None,
            selection_autoscroll: None,
            swallow_left_gesture: false,
            sidebar_offset: 0,
            attention_sidebar_offset: 0,
            sidebar_follow_selected: true,
            sidebar_last_selected: None,
            sidebar_hit_areas: Vec::new(),
            sidebar_press: None,
            last_render_at: None,
            message: None,
            clipboard_feedback: None,
            config_path: None,
            configured_targets: Vec::new(),
            resolved_targets: BTreeMap::new(),
            configuration_dirty: false,
            target_manager: None,
            target_test_sender: None,
            next_target_test_request: 1,
            agent_navigator: None,
            attention: AttentionIndex::default(),
            attention_center: None,
            notification_queue: NotificationQueue::default(),
            notification_sender: None,
            notification_inflight: false,
            notification_error_reported: false,
            command_palette: None,
            context_menu: None,
            text_prompt: None,
            close_confirmation: None,
            herdr_action_inflight: false,
        }
    }
}

pub async fn run(config: Config, config_path: PathBuf) -> Result<()> {
    let configured_targets = config.targets.clone();
    let configured_notifications = config.notifications.clone();
    let mut active_config = expand_discovered_sessions(config).await;
    let (mut terminal, _guard) = enter_terminal()?;
    // The frontend keeps no target map: nothing it does reaches a Herdr server
    // directly any more. What it still needs from configuration is the
    // transport for its own connection tests, and the list the target manager
    // shows.
    let mut transport_config = active_config.transport.clone();
    // The frontend hosts its own daemon, so a single-machine install still runs
    // as one command with no socket and no service to operate. It is the same
    // daemon a phone would reach, spoken over an in-memory pipe rather than a
    // socket, so both paths exercise one implementation.
    //
    let daemon_options = DaemonOptions::discover()?;
    let daemon = spawn_in_process(
        active_config.clone(),
        Some(config_path.clone()),
        daemon_options,
    );
    let (client, mut daemon_events) = Client::attach(&daemon, "super-herdr-tui").await?;
    client.subscribe_state();
    let mut updates = client.state();
    let (input_sender, mut input) = mpsc::unbounded_channel();
    let (config_refresh_sender, mut config_refreshes) = mpsc::unbounded_channel();
    let (target_test_sender, mut target_test_events) = mpsc::unbounded_channel();
    let (notification_sender, mut notification_events) = mpsc::unbounded_channel();
    spawn_input_reader(input_sender);
    let mut ticks = interval(Duration::from_millis(100));
    let mut selection_ticks = interval(SELECTION_AUTOSCROLL_INTERVAL);
    let mut config_refresh_ticks = interval(CONFIG_REFRESH_INTERVAL);
    config_refresh_ticks.tick().await;
    let mut config_refresh_inflight = false;
    let mut input_decoder = InputDecoder::default();
    let state_store = UiStateStore::discover()?;
    let restore_pending = state_store.load().unwrap_or_default().selected_pane;
    // Attention is the daemon's: it owns the durable index, and this is a
    // mirror of it. Seeding it from the wire rather than from the file means a
    // frontend and a daemon can never disagree about what has been read.
    let attention = AttentionIndex::default();
    let last_attention_event_id = None;
    let mut app = App {
        restore_pending,
        state_store: Some(state_store),
        attention,
        notification_queue: NotificationQueue::new(
            configured_notifications,
            last_attention_event_id,
        ),
        notification_sender: Some(notification_sender),
        config_path: Some(config_path.clone()),
        configured_targets,
        resolved_targets: resolved_targets(&active_config),
        target_test_sender: Some(target_test_sender),
        client: Some(client.commands()),
        ..App::default()
    };
    let mut should_draw = true;

    let result = loop {
        let now = Instant::now();
        if should_draw
            && app
                .last_render_at
                .is_none_or(|last_render| now.duration_since(last_render) >= MIN_RENDER_INTERVAL)
        {
            let state = updates.borrow().clone();
            refresh_attention(&mut app);
            reconcile_selection(&state, &mut app);
            ensure_routes(&state, &mut terminal, &mut app)?;
            let frame_area: Rect = terminal
                .size()
                .context("failed to read terminal size")?
                .into();
            app.last_frame_area = Some(frame_area);
            let (sidebar_area, _, _) = ui_areas(frame_area);
            update_sidebar_hit_areas(&state, &mut app, sidebar_area);
            terminal
                .draw(|frame| render(frame, &state, &app))
                .context("failed to render the terminal UI")?;
            app.last_render_at = Some(Instant::now());
            should_draw = false;
        }

        let next_render_at = should_draw.then(|| {
            app.last_render_at
                .map(|last_render| last_render + MIN_RENDER_INTERVAL)
                .unwrap_or_else(Instant::now)
        });
        let render_wakeup =
            next_render_at.unwrap_or_else(|| Instant::now() + Duration::from_secs(24 * 60 * 60));

        tokio::select! {
            biased;
            byte = input.recv() => {
                let Some(byte) = byte else {
                    break Ok(());
                };
                if let Some(event) = input_decoder.push(byte) {
                    if handle_decoded_input(
                        event,
                        &updates.borrow(),
                        &transport_config,
                        &mut app,
                    ).await? {
                        break Ok(());
                    }
                    should_draw = true;
                }
            }
            _ = sleep_until(render_wakeup.into()), if next_render_at.is_some() => {}
            changed = updates.changed() => {
                if changed.is_err() {
                    break Ok(());
                }
                should_draw = true;
            }
            refresh = config_refreshes.recv() => {
                config_refresh_inflight = false;
                match refresh {
                    Some(ConfigRefresh::Ready(refreshed)) => {
                        let ConfigRefreshed { configured, expanded } = *refreshed;
                        if app
                            .notification_queue
                            .reconfigure(configured.notifications.clone())
                        {
                            app.notification_error_reported = false;
                        }
                        app.configured_targets = configured.targets.clone();
                        app.resolved_targets = resolved_targets(&expanded);
                        // Supervisors belong to the daemon, which refreshes them
                        // from the same file on its own schedule. What the
                        // frontend still needs from a reload is the local
                        // clipboard transport and the list the target manager
                        // shows.
                        if federation_routes_changed(&active_config, &expanded) {
                            transport_config = expanded.transport.clone();
                            app.message = Some("refreshed configured hosts and running sessions".to_owned());
                        }
                        active_config = expanded;
                        if let Some(manager) = app.target_manager.as_mut()
                            && matches!(&manager.mode, TargetManagerMode::List)
                        {
                            manager.targets = app.configured_targets.clone();
                            manager.selected = manager
                                .selected
                                .min(manager.targets.len().saturating_sub(1));
                        }
                    }
                    Some(ConfigRefresh::Failed(error)) => {
                        app.message = Some(format!("configuration refresh failed: {error}"));
                    }
                    None => {}
                }
                should_draw = true;
            }
            event = target_test_events.recv() => {
                if let Some(event) = event {
                    apply_target_test_event(event, &mut app);
                    should_draw = true;
                }
            }
            event = notification_events.recv() => {
                if let Some(event) = event {
                    match event {
                        NotificationEvent::Delivered(result) => {
                            app.notification_inflight = false;
                            match result {
                                Ok(()) => app.notification_error_reported = false,
                                Err(error) if !app.notification_error_reported => {
                                    app.notification_error_reported = true;
                                    app.message = Some(format!("desktop notification unavailable: {error}"));
                                }
                                Err(_) => {}
                            }
                        }
                        NotificationEvent::Activated(pane) => {
                            jump_from_notification(&updates.borrow(), &mut app, pane);
                        }
                    }
                    should_draw = true;
                }
            }
            event = daemon_events.recv() => {
                let Some(event) = event else {
                    break Err(anyhow::anyhow!("the daemon stopped serving this frontend"));
                };
                handle_daemon_event(event, &mut app);
                // Frames are drained in bounded batches so a busy pane cannot
                // starve input.
                for _ in 1..ROUTE_EVENT_DRAIN_LIMIT {
                    let Ok(event) = daemon_events.try_recv() else {
                        break;
                    };
                    handle_daemon_event(event, &mut app);
                }
                should_draw = true;
            }
            _ = selection_ticks.tick() => {
                if tick_selection_autoscroll(&mut app).await? {
                    should_draw = true;
                }
            }
            _ = ticks.tick() => {
                if app.configuration_dirty && !config_refresh_inflight {
                    app.configuration_dirty = false;
                    config_refresh_inflight = true;
                    spawn_config_refresh(config_path.clone(), config_refresh_sender.clone());
                }
                if let Some(event) = input_decoder.flush_expired() {
                    if handle_decoded_input(
                        event,
                        &updates.borrow(),
                        &transport_config,
                        &mut app,
                    ).await? {
                        break Ok(());
                    }
                    should_draw = true;
                }
                let current_size = terminal.size().context("failed to read terminal size")?;
                let frame_area = current_size.into();
                let (_, _, terminal_area) = ui_areas(frame_area);
                app.last_frame_area = Some(frame_area);
                if app.last_terminal_area != Some(terminal_area) {
                    app.last_terminal_area = Some(terminal_area);
                    should_draw = true;
                }
                let before = app.route_retry_after.len();
                app.route_retry_after.retain(|_, retry| *retry > Instant::now());
                if app.route_retry_after.len() != before {
                    should_draw = true;
                }
                if app
                    .clipboard_feedback
                    .as_ref()
                    .is_some_and(|feedback| feedback.expires_at <= Instant::now())
                {
                    app.clipboard_feedback = None;
                    should_draw = true;
                }
                if !app.notification_inflight
                    && let Some(delivery) = app.notification_queue.take_ready(Instant::now())
                {
                    spawn_notification_delivery(delivery, &mut app);
                }
            }
            _ = config_refresh_ticks.tick() => {
                app.configuration_dirty = true;
            }
        }
    };

    // Dropping the client releases every lease and route it held; the daemon
    // then stops with it. No Herdr session is touched either way.
    drop(client);
    daemon.shutdown().await;
    result
}

fn enter_terminal() -> Result<(Terminal<CrosstermBackend<Stdout>>, TerminalGuard)> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("the tui command requires an interactive terminal");
    }
    enable_raw_mode().context("failed to enable raw terminal mode")?;
    let guard = TerminalGuard;
    let mut output = io::stdout();
    if let Err(error) = execute!(output, EnterAlternateScreen, EnableBracketedPaste, Hide) {
        drop(guard);
        return Err(error).context("failed to enter the alternate screen");
    }
    if let Err(error) = output
        .write_all(MOUSE_CAPTURE_ENABLE)
        .and_then(|()| output.flush())
    {
        drop(guard);
        return Err(error).context("failed to enable button-motion mouse capture");
    }
    let mut terminal = Terminal::new(CrosstermBackend::new(output))
        .context("failed to initialize the terminal backend")?;
    terminal
        .autoresize()
        .context("failed to size the terminal")?;
    Ok((terminal, guard))
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            Show,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

fn spawn_input_reader(sender: mpsc::UnboundedSender<u8>) {
    thread::spawn(move || {
        let mut input = io::stdin().lock();
        let mut byte = [0_u8; 1];
        while input.read_exact(&mut byte).is_ok() {
            if sender.send(byte[0]).is_err() {
                break;
            }
        }
    });
}

fn spawn_config_refresh(path: PathBuf, sender: mpsc::UnboundedSender<ConfigRefresh>) {
    tokio::spawn(async move {
        let refresh = match Config::load(Some(&path)) {
            Ok((configured, _)) => {
                let expanded = expand_discovered_sessions(configured.clone()).await;
                ConfigRefresh::Ready(Box::new(ConfigRefreshed {
                    configured,
                    expanded,
                }))
            }
            Err(error) => ConfigRefresh::Failed(error.to_string()),
        };
        let _ = sender.send(refresh);
    });
}

fn spawn_notification_delivery(delivery: NotificationDelivery, app: &mut App) {
    let Some(sender) = app.notification_sender.clone() else {
        if !app.notification_error_reported {
            app.notification_error_reported = true;
            app.message = Some("desktop notification delivery is unavailable".to_owned());
        }
        return;
    };
    app.notification_inflight = true;
    tokio::spawn(async move {
        let pending = match notifications::dispatch(&delivery).await {
            Ok(pending) => pending,
            Err(error) => {
                let _ = sender.send(NotificationEvent::Delivered(Err(error.to_string())));
                return;
            }
        };
        // Delivery is complete once the desktop accepts the notification, so
        // the next one is never gated on someone reading this one.
        let _ = sender.send(NotificationEvent::Delivered(Ok(())));
        if let Ok(Some(pane)) = notifications::wait_for_activation(pending).await {
            let _ = sender.send(NotificationEvent::Activated(pane));
        }
    });
}

/// Route a clicked notification to its qualified pane. The identifier is only
/// as fresh as the notification, so a pane that has since gone marks its events
/// read instead of moving the selection, exactly as the attention feed does.
fn jump_from_notification(state: &FederationState, app: &mut App, pane: PaneId) {
    if pane_is_live(state, &pane) {
        let target = pane.target_session();
        select_pane(app, pane);
        app.message = Some(format!("jumped to the notified pane on {target}"));
        return;
    }
    mark_attention_seen(app, &pane);
    app.message = Some(format!(
        "the notified pane on {} is no longer live",
        pane.target_session()
    ));
}

async fn handle_decoded_input(
    input: DecodedInput,
    state: &FederationState,
    transport_config: &crate::config::TransportConfig,
    app: &mut App,
) -> Result<bool> {
    match input {
        DecodedInput::Bytes(bytes) => {
            if app.command_palette.is_some()
                || app.context_menu.is_some()
                || app.target_manager.is_some()
            {
                let navigation = match bytes.as_slice() {
                    b"\x1b[A" => Some(0x10),
                    b"\x1b[B" => Some(0x0e),
                    _ => None,
                };
                if let Some(key) = navigation {
                    return handle_input(key, state, transport_config, app).await;
                }
            }
            for byte in bytes {
                if handle_input(byte, state, transport_config, app).await? {
                    return Ok(true);
                }
            }
        }
        DecodedInput::Mouse(mouse) => handle_mouse(mouse, state, transport_config, app).await?,
        DecodedInput::Paste(bytes) => {
            if app.mode != InputMode::Terminal
                || app.target_manager.is_some()
                || app.agent_navigator.is_some()
                || app.attention_center.is_some()
                || app.command_palette.is_some()
                || app.context_menu.is_some()
                || app.text_prompt.is_some()
                || app.close_confirmation.is_some()
            {
                app.message = Some("close the active Super-Herdr dialog before pasting".to_owned());
            } else {
                let text = match String::from_utf8(bytes) {
                    Ok(text) => text,
                    Err(_) => {
                        app.message =
                            Some("terminal paste was not valid UTF-8; paste not sent".to_owned());
                        return Ok(false);
                    }
                };
                let characters = text.chars().count();
                paste_text_into_selected(
                    state,
                    app,
                    text,
                    format!("pasted {characters} characters from terminal clipboard"),
                )
                .await?;
            }
        }
        DecodedInput::PasteTooLarge => {
            app.message = Some(format!(
                "terminal paste exceeded the {} MiB limit; paste not sent",
                MAX_CLIPBOARD_BYTES / (1024 * 1024)
            ));
        }
    }
    Ok(false)
}

async fn handle_mouse(
    mouse: MouseInput,
    state: &FederationState,
    transport_config: &crate::config::TransportConfig,
    app: &mut App,
) -> Result<()> {
    if app.context_menu.is_some() {
        handle_context_menu_mouse(mouse, state, app);
        return Ok(());
    }
    if app.target_manager.is_some() {
        if let Some(frame_area) = app.last_frame_area {
            handle_target_manager_mouse(mouse, frame_area, transport_config, app)?;
        }
        return Ok(());
    }
    if app.mode != InputMode::Terminal
        || app.agent_navigator.is_some()
        || app.attention_center.is_some()
        || app.command_palette.is_some()
        || app.text_prompt.is_some()
        || app.close_confirmation.is_some()
    {
        return Ok(());
    }
    let Some(frame_area) = app.last_frame_area else {
        return Ok(());
    };
    let (sidebar_area, tab_area, terminal_area) = ui_areas(frame_area);
    let outer = Position::new(mouse.column.saturating_sub(1), mouse.row.saturating_sub(1));

    if app.swallow_left_gesture {
        if mouse.is_left_release() {
            finish_ui_left_gesture(app);
        }
        return Ok(());
    }

    if let Some(selection) = app.selection.as_ref()
        && (mouse.is_left_motion() || mouse.is_left_release())
    {
        let pane = selection.pane.clone();
        if let Some(viewport_position) =
            clamped_pane_position(outer, state, app, terminal_area, &pane)
        {
            let viewport_offset = selection.viewport_offset;
            let position = CellPosition::from_viewport(
                u16::try_from(viewport_position.row).unwrap_or_default(),
                viewport_position.column,
                viewport_offset,
            );
            if mouse.is_left_motion() {
                let autoscroll_direction =
                    selection_autoscroll_direction(outer, state, app, terminal_area, &pane);
                let dragging = {
                    let selection = app.selection.as_mut().expect("selection was checked above");
                    selection.head = position;
                    selection.dragging |=
                        selection.head != selection.anchor || autoscroll_direction.is_some();
                    selection.dragging
                };
                let pending_lines = app
                    .selection_autoscroll
                    .as_ref()
                    .and_then(|current| {
                        (current.pane == pane && Some(current.direction) == autoscroll_direction)
                            .then_some(current.pending_lines)
                    })
                    .unwrap_or(0);
                app.selection_autoscroll =
                    dragging
                        .then_some(autoscroll_direction)
                        .flatten()
                        .map(|direction| SelectionAutoscroll {
                            pane,
                            direction,
                            column: position.column,
                            pending_lines,
                        });
            } else {
                app.selection_autoscroll = None;
                let mut selection = app.selection.take().expect("selection was checked above");
                match selection.finish(position, viewport_position, mouse) {
                    SelectionFinish::Retain => {
                        copy_terminal_selection(&mut selection, app)?;
                        app.selection = Some(selection);
                    }
                    SelectionFinish::ForwardClick(events) => {
                        send_mouse_inputs(&pane, &events, state, app).await?;
                    }
                    SelectionFinish::Clear => {}
                }
            }
        }
        return Ok(());
    }

    if sidebar_area.contains(outer) {
        if let Some(direction) = mouse.scroll_direction() {
            let (sessions_area, attention_area) = sidebar_areas(sidebar_area);
            if attention_area.contains(outer) {
                scroll_attention_sidebar(state, app, sidebar_area, direction);
            } else if sessions_area.contains(outer) {
                scroll_session_sidebar(state, app, sidebar_area, direction);
            }
        } else if mouse.is_right_press() {
            if let Some((pane, target)) = app
                .sidebar_hit_areas
                .iter()
                .find(|hit| hit.area.contains(outer))
                .map(|hit| (hit.pane.clone(), hit.context.clone()))
                && let Some(target) = target
            {
                if let Some(pane) = pane {
                    select_pane(app, pane);
                }
                open_context_menu(state, target, outer, app);
            }
        } else if mouse.is_left_press() {
            app.sidebar_press = app
                .sidebar_hit_areas
                .iter()
                .find(|hit| hit.area.contains(outer))
                .and_then(|hit| hit.pane.clone());
            app.swallow_left_gesture = true;
        }
        return Ok(());
    }
    if tab_area.contains(outer) {
        if mouse.is_right_press() {
            if let Some(tab) = tab_at_column(
                state,
                app.selected_pane.as_ref(),
                outer.x.saturating_sub(tab_area.x),
            ) {
                if let Some((snapshot, _)) =
                    selected_snapshot_and_pane(state, app.selected_pane.as_ref())
                {
                    select_tab(snapshot, &tab, app);
                }
                open_context_menu(state, ContextTarget::Tab(tab), outer, app);
            }
        } else if mouse.is_left_press() {
            if let Some(tab) = tab_at_column(
                state,
                app.selected_pane.as_ref(),
                outer.x.saturating_sub(tab_area.x),
            ) && let Some((snapshot, _)) =
                selected_snapshot_and_pane(state, app.selected_pane.as_ref())
            {
                select_tab(snapshot, &tab, app);
            }
            app.swallow_left_gesture = true;
        } else if let Some(direction) = mouse.scroll_direction() {
            cycle_tab(
                state,
                app,
                match direction {
                    TerminalScrollDirection::Up => -1,
                    TerminalScrollDirection::Down => 1,
                },
            );
        }
        return Ok(());
    }
    let Some((pane, position, local_mouse)) = mouse_pane_position(mouse, state, app, terminal_area)
    else {
        return Ok(());
    };

    if mouse.is_right_press() {
        if app.selected_pane.as_ref() != Some(&pane) {
            select_pane(app, pane.clone());
        }
        open_context_menu(state, ContextTarget::Pane(pane), outer, app);
        return Ok(());
    }

    if app.selected_pane.as_ref() != Some(&pane) {
        if mouse.is_left_press() {
            select_pane(app, pane);
            app.swallow_left_gesture = true;
        }
        return Ok(());
    }

    if mouse.is_vertical_wheel() {
        app.selection = None;
        app.selection_autoscroll = None;
        return send_terminal_scroll(&pane, local_mouse, state, app).await;
    }

    if mouse.is_left_press() {
        let forwarded_click =
            (pane_reports_mouse(app, &pane) && !mouse.shift()).then_some(local_mouse);
        let position = CellPosition::from_viewport(
            u16::try_from(position.row).unwrap_or_default(),
            position.column,
            0,
        );
        app.selection_autoscroll = None;
        let mut selection = TerminalSelection {
            pane,
            anchor: position,
            head: position,
            viewport_offset: 0,
            captured_rows: BTreeMap::new(),
            dragging: false,
            forwarded_click,
        };
        if let Some(route) = app.routes.get(&selection.pane) {
            capture_selection_viewport(&mut selection, route.parser.screen());
        }
        app.selection = Some(selection);
        return Ok(());
    }

    send_mouse_inputs(&pane, &[local_mouse], state, app).await
}

fn finish_ui_left_gesture(app: &mut App) {
    app.swallow_left_gesture = false;
    if let Some(pane) = app.sidebar_press.take() {
        select_pane(app, pane);
    }
}

fn pane_reports_mouse(app: &App, pane: &PaneId) -> bool {
    app.routes.get(pane).is_some_and(|route| {
        mouse_passthrough_enabled(
            route.parser.screen().mouse_protocol_mode(),
            route.access == TerminalAccess::Control,
        )
    })
}

fn mouse_passthrough_enabled(mode: vt100::MouseProtocolMode, writable: bool) -> bool {
    writable && mode != vt100::MouseProtocolMode::None
}

fn mouse_pane_position(
    mouse: MouseInput,
    state: &FederationState,
    app: &App,
    terminal_area: Rect,
) -> Option<(PaneId, CellPosition, MouseInput)> {
    let outer_column = mouse.column.checked_sub(1)?;
    let outer_row = mouse.row.checked_sub(1)?;
    let outer = Position::new(outer_column, outer_row);
    visible_pane_areas(state, app.selected_pane.as_ref(), terminal_area)
        .into_iter()
        .find_map(|(pane, area)| {
            let selected = app.selected_pane.as_ref() == Some(&pane);
            let inner = pane_block(
                &pane,
                selected,
                app.routes.get(&pane).map(|route| route.access),
            )
            .inner(area);
            inner.contains(outer).then(|| {
                let position = CellPosition {
                    row: i64::from(outer_row - inner.y),
                    column: outer_column - inner.x,
                };
                (
                    pane,
                    position,
                    MouseInput {
                        column: position.column + 1,
                        row: u16::try_from(position.row)
                            .unwrap_or_default()
                            .saturating_add(1),
                        ..mouse
                    },
                )
            })
        })
}

fn clamped_pane_position(
    outer: Position,
    state: &FederationState,
    app: &App,
    terminal_area: Rect,
    pane: &PaneId,
) -> Option<CellPosition> {
    let area = visible_pane_areas(state, app.selected_pane.as_ref(), terminal_area)
        .into_iter()
        .find_map(|(candidate, area)| (candidate == *pane).then_some(area))?;
    let inner = pane_block(
        pane,
        app.selected_pane.as_ref() == Some(pane),
        app.routes.get(pane).map(|route| route.access),
    )
    .inner(area);
    if inner.width == 0 || inner.height == 0 {
        return None;
    }
    Some(CellPosition {
        row: i64::from(
            outer
                .y
                .clamp(inner.y, inner.y + inner.height - 1)
                .saturating_sub(inner.y),
        ),
        column: outer
            .x
            .clamp(inner.x, inner.x + inner.width - 1)
            .saturating_sub(inner.x),
    })
}

fn selection_autoscroll_direction(
    outer: Position,
    state: &FederationState,
    app: &App,
    terminal_area: Rect,
    pane: &PaneId,
) -> Option<SelectionAutoscrollDirection> {
    let inner = pane_inner_area(state, app, terminal_area, pane)?;
    if outer.y <= inner.y {
        Some(SelectionAutoscrollDirection::Up)
    } else if outer.y >= inner.y + inner.height.saturating_sub(1) {
        Some(SelectionAutoscrollDirection::Down)
    } else {
        None
    }
}

fn pane_inner_area(
    state: &FederationState,
    app: &App,
    terminal_area: Rect,
    pane: &PaneId,
) -> Option<Rect> {
    let area = visible_pane_areas(state, app.selected_pane.as_ref(), terminal_area)
        .into_iter()
        .find_map(|(candidate, area)| (candidate == *pane).then_some(area))?;
    let inner = pane_block(
        pane,
        app.selected_pane.as_ref() == Some(pane),
        app.routes.get(pane).map(|route| route.access),
    )
    .inner(area);
    (inner.width > 0 && inner.height > 0).then_some(inner)
}

async fn tick_selection_autoscroll(app: &mut App) -> Result<bool> {
    let Some(autoscroll) = app.selection_autoscroll.clone() else {
        return Ok(false);
    };
    if !app
        .selection
        .as_ref()
        .is_some_and(|selection| selection.pane == autoscroll.pane && selection.dragging)
    {
        app.selection_autoscroll = None;
        return Ok(false);
    }
    let Some(route) = app.routes.get(&autoscroll.pane) else {
        app.selection_autoscroll = None;
        return Ok(false);
    };
    let direction = match autoscroll.direction {
        SelectionAutoscrollDirection::Up => TerminalScrollDirection::Up,
        SelectionAutoscrollDirection::Down => TerminalScrollDirection::Down,
    };
    let row = match autoscroll.direction {
        SelectionAutoscrollDirection::Up => 0,
        SelectionAutoscrollDirection::Down => route.rows.saturating_sub(1),
    };
    let Some(client) = app.client.clone().filter(|_| {
        app.routes
            .get(&autoscroll.pane)
            .is_some_and(|route| route.access == TerminalAccess::Control)
    }) else {
        app.selection_autoscroll = None;
        return Ok(false);
    };
    client.scroll_pane(
        autoscroll.pane.clone(),
        direction,
        1,
        autoscroll.column,
        row,
        0,
    );
    if let Some(current) = app.selection_autoscroll.as_mut()
        && current.pane == autoscroll.pane
        && current.direction == autoscroll.direction
    {
        current.pending_lines = current.pending_lines.saturating_add(1);
    }
    Ok(false)
}

/// Paste a path the daemon verified into the selected pane.
fn paste_verified_path(app: &mut App, path: &str, feedback: String) {
    let Some(selected) = app.selected_pane.clone() else {
        app.message = Some("no pane is selected for the uploaded path".to_owned());
        return;
    };
    let Some(route) = app.routes.get(&selected) else {
        app.message = Some("terminal control route closed during media upload".to_owned());
        return;
    };
    if route.access != TerminalAccess::Control {
        app.message = Some("read-only: the uploaded path was not pasted".to_owned());
        return;
    }
    let bracketed = route.parser.screen().bracketed_paste();
    let Ok(payload) = terminal_paste_payload(path.as_bytes(), bracketed) else {
        app.message = Some("the uploaded path could not be encoded for paste".to_owned());
        return;
    };
    let Some(client) = app.client.clone() else {
        return;
    };
    client.send_input(selected, payload);
    app.clipboard_feedback = Some(ClipboardFeedback {
        text: feedback,
        expires_at: Instant::now() + CLIPBOARD_FEEDBACK_DURATION,
    });
}

/// Mark a pane's events read here and ask the daemon to do the same.
///
/// The local edit is optimistic so a badge clears in the frame the person acted
/// in rather than a round trip later. It is not authoritative: the daemon
/// answers every change with the whole history, so a request that does not land
/// is corrected back rather than silently believed.
fn mark_attention_seen(app: &mut App, pane: &PaneId) {
    if app.attention.mark_seen_for_pane(pane)
        && let Some(client) = app.client.as_ref()
    {
        client.mark_attention_seen(pane.clone());
    }
}

/// Hand every held event to the notification queue, which decides for itself
/// what is new and what is worth delivering.
fn refresh_attention(app: &mut App) {
    let events = app.attention.events().cloned().collect::<Vec<_>>();
    let now = Instant::now();
    for event in &events {
        app.notification_queue.enqueue(event, now);
    }
}

fn select_pane(app: &mut App, pane: PaneId) {
    app.selection_explicit = true;
    app.restore_pending = None;
    app.selection = None;
    app.selection_autoscroll = None;
    app.sidebar_press = None;
    app.sidebar_follow_selected = true;
    app.selected_pane = Some(pane.clone());
    app.route_retry_after.remove(&pane);
    app.message = None;
    mark_attention_seen(app, &pane);
    if let Some(store) = app.state_store.as_ref()
        && store.save(&UiState::selected_pane(pane)).is_err()
    {
        app.message = Some("failed to persist the selected pane".to_owned());
    }
}

async fn send_terminal_scroll(
    pane: &PaneId,
    mouse: MouseInput,
    state: &FederationState,
    app: &mut App,
) -> Result<()> {
    let Some(direction) = mouse.scroll_direction() else {
        return Ok(());
    };
    let Some(route) = app.routes.get(pane) else {
        return Ok(());
    };
    let Some(target) = state.targets.get(&pane.target_session()) else {
        return Ok(());
    };
    if !target.accepts_generation(route.generation) {
        app.routes.remove(pane);
        app.message = Some("control route became stale".to_owned());
        return Ok(());
    }

    let Some(client) = app
        .client
        .clone()
        .filter(|_| route.access == TerminalAccess::Control)
    else {
        app.message =
            Some("read-only: another Herdr client owns control; retrying automatically".to_owned());
        return Ok(());
    };
    client.scroll_pane(
        pane.clone(),
        direction,
        MOUSE_SCROLL_LINES,
        mouse.column.saturating_sub(1),
        mouse.row.saturating_sub(1),
        mouse.key_modifiers(),
    );
    Ok(())
}

async fn send_mouse_inputs(
    pane: &PaneId,
    events: &[MouseInput],
    state: &FederationState,
    app: &mut App,
) -> Result<()> {
    let Some(route) = app.routes.get(pane) else {
        return Ok(());
    };
    let Some(target) = state.targets.get(&pane.target_session()) else {
        return Ok(());
    };
    if !target.accepts_generation(route.generation) {
        app.routes.remove(pane);
        app.message = Some("control route became stale".to_owned());
        return Ok(());
    }
    let mode = route.parser.screen().mouse_protocol_mode();
    let encoding = route.parser.screen().mouse_protocol_encoding();
    let bytes = events
        .iter()
        .copied()
        .filter(|event| mouse_event_allowed(mode, *event))
        .filter_map(|event| encode_mouse_event(event, encoding))
        .flatten()
        .collect::<Vec<_>>();
    if bytes.is_empty() {
        return Ok(());
    }

    let Some(client) = app
        .client
        .clone()
        .filter(|_| route.access == TerminalAccess::Control)
    else {
        app.message =
            Some("read-only: another Herdr client owns control; retrying automatically".to_owned());
        return Ok(());
    };
    client.send_input(pane.clone(), bytes);
    Ok(())
}

fn mouse_event_allowed(mode: vt100::MouseProtocolMode, mouse: MouseInput) -> bool {
    match mode {
        vt100::MouseProtocolMode::None => false,
        vt100::MouseProtocolMode::Press => !mouse.release && !mouse.is_motion(),
        vt100::MouseProtocolMode::PressRelease => !mouse.is_motion(),
        vt100::MouseProtocolMode::ButtonMotion => !mouse.is_motion() || mouse.code & 0b11 != 0b11,
        vt100::MouseProtocolMode::AnyMotion => true,
    }
}

fn encode_mouse_event(
    mouse: MouseInput,
    encoding: vt100::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    match encoding {
        vt100::MouseProtocolEncoding::Sgr => Some(
            format!(
                "\x1b[<{};{};{}{}",
                mouse.code,
                mouse.column,
                mouse.row,
                if mouse.release { 'm' } else { 'M' }
            )
            .into_bytes(),
        ),
        vt100::MouseProtocolEncoding::Default => {
            let code = if mouse.release {
                (mouse.code & !0b11) | 0b11
            } else {
                mouse.code
            };
            Some(vec![
                0x1b,
                b'[',
                b'M',
                u8::try_from(code.checked_add(32)?).ok()?,
                u8::try_from(mouse.column.checked_add(32)?).ok()?,
                u8::try_from(mouse.row.checked_add(32)?).ok()?,
            ])
        }
        vt100::MouseProtocolEncoding::Utf8 => {
            let code = if mouse.release {
                (mouse.code & !0b11) | 0b11
            } else {
                mouse.code
            };
            let mut bytes = b"\x1b[M".to_vec();
            for value in [code, mouse.column, mouse.row] {
                let character = char::from_u32(u32::from(value.checked_add(32)?))?;
                let mut encoded = [0; 4];
                bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
            }
            Some(bytes)
        }
    }
}

fn copy_terminal_selection(selection: &mut TerminalSelection, app: &mut App) -> Result<()> {
    let Some(route) = app.routes.get(&selection.pane) else {
        return Ok(());
    };
    capture_selection_viewport(selection, route.parser.screen());
    let text = selected_terminal_text(selection);
    if text.is_empty() {
        app.message = Some("selection is empty".to_owned());
        return Ok(());
    }
    if text.len() > MAX_CLIPBOARD_BYTES {
        app.message = Some("selection is too large to copy".to_owned());
        return Ok(());
    }
    let clipboard = clipboard::write_text(&text)?;
    app.clipboard_feedback = Some(ClipboardFeedback {
        text: clipboard.feedback(text.chars().count()),
        expires_at: Instant::now() + CLIPBOARD_FEEDBACK_DURATION,
    });
    Ok(())
}

fn selected_terminal_text(selection: &TerminalSelection) -> String {
    let start = selection.anchor.min(selection.head);
    let end = selection.anchor.max(selection.head);
    let mut lines = Vec::new();
    for row in start.row..=end.row {
        let Some(cells) = selection.captured_rows.get(&row) else {
            lines.push(String::new());
            continue;
        };
        let first_column = usize::from(if row == start.row { start.column } else { 0 });
        let last_column = usize::from(if row == end.row {
            end.column
        } else {
            u16::try_from(cells.len().saturating_sub(1)).unwrap_or(u16::MAX)
        })
        .min(cells.len().saturating_sub(1));
        let mut line = String::new();
        if first_column <= last_column {
            for cell in &cells[first_column..=last_column] {
                if let CapturedCell::Text(text) = cell {
                    line.push_str(text);
                }
            }
        }
        lines.push(line.trim_end_matches(' ').to_owned());
    }
    lines.join("\n")
}

fn capture_selection_viewport(selection: &mut TerminalSelection, screen: &vt100::Screen) {
    capture_selection_rows(selection, capture_screen_rows(screen));
}

fn capture_selection_rows(selection: &mut TerminalSelection, rows: Vec<CapturedRow>) {
    for (viewport_row, row) in rows.into_iter().enumerate() {
        let Ok(viewport_row) = u16::try_from(viewport_row) else {
            break;
        };
        let logical_row = i64::from(viewport_row) - selection.viewport_offset;
        selection.captured_rows.insert(logical_row, row);
    }
}

fn capture_screen_rows(screen: &vt100::Screen) -> Vec<CapturedRow> {
    let (rows, columns) = screen.size();
    (0..rows)
        .map(|row| {
            (0..columns)
                .map(|column| match screen.cell(row, column) {
                    Some(cell) if cell.is_wide_continuation() => CapturedCell::WideContinuation,
                    Some(cell) if cell.has_contents() => {
                        CapturedCell::Text(cell.contents().to_owned())
                    }
                    _ => CapturedCell::Text(" ".to_owned()),
                })
                .collect()
        })
        .collect()
}

async fn handle_input(
    key: u8,
    state: &FederationState,
    transport_config: &crate::config::TransportConfig,
    app: &mut App,
) -> Result<bool> {
    if app.context_menu.is_some() {
        return handle_context_menu_input(key, state, app);
    }
    if app.close_confirmation.is_some() {
        return handle_close_confirmation_input(key, app);
    }
    if app.text_prompt.is_some() {
        return handle_text_prompt_input(key, app);
    }
    if app.command_palette.is_some() {
        return handle_command_palette_input(key, state, app);
    }
    if app.target_manager.is_some() {
        return handle_target_manager_input(key, transport_config, app);
    }
    if app.agent_navigator.is_some() {
        return handle_agent_navigator_input(key, state, app);
    }
    if app.attention_center.is_some() {
        return handle_attention_center_input(key, state, app);
    }
    match app.mode {
        InputMode::Terminal if key == PREFIX_KEY => {
            app.selection = None;
            app.selection_autoscroll = None;
            app.mode = InputMode::Prefix;
        }
        InputMode::Terminal if key == HERDR_PREFIX_KEY => {
            app.selection = None;
            app.selection_autoscroll = None;
            app.mode = InputMode::HerdrPrefix;
        }
        InputMode::Terminal => {
            app.selection = None;
            app.selection_autoscroll = None;
            let Some(selected) = app.selected_pane.clone() else {
                app.message = Some("no terminal pane is selected".to_owned());
                return Ok(false);
            };
            let Some(route) = app.routes.get(&selected) else {
                app.message = Some("selected pane has no control route".to_owned());
                return Ok(false);
            };
            let Some(target) = state.targets.get(&route.pane.target_session()) else {
                app.routes.remove(&selected);
                return Ok(false);
            };
            if !target.accepts_generation(route.generation) {
                app.routes.remove(&selected);
                app.message = Some("control route became stale".to_owned());
                return Ok(false);
            }
            let controlling = app
                .routes
                .get(&selected)
                .is_some_and(|route| route.access == TerminalAccess::Control);
            let Some(client) = app.client.clone().filter(|_| controlling) else {
                app.message = Some(
                    "read-only: another Herdr client owns control; retrying automatically"
                        .to_owned(),
                );
                return Ok(false);
            };
            client.send_input(selected.clone(), vec![key]);
        }
        InputMode::Prefix => {
            match key {
                b'q' => return Ok(true),
                b'j' => cycle_pane(state, app, 1),
                b'k' => cycle_pane(state, app, -1),
                b'n' => cycle_tab(state, app, 1),
                b'p' => cycle_tab(state, app, -1),
                b'v' => paste_system_clipboard(state, app).await?,
                b'i' => paste_clipboard_media(state, app).await?,
                b'h' => {
                    app.target_manager = Some(TargetManager::new(app.configured_targets.clone()));
                }
                b'a' => app.agent_navigator = Some(AgentNavigator::default()),
                b'e' => app.attention_center = Some(AttentionCenter::default()),
                // Pairing starts from a client that is already trusted, so the
                // code appears on a screen someone already has rather than in a
                // file or a log.
                b'P' => match app.client.as_ref() {
                    Some(client) => {
                        client.request_pairing_code();
                        app.message = Some("asking the daemon for a pairing code…".to_owned());
                    }
                    None => app.message = Some("pairing is unavailable".to_owned()),
                },
                b'd' => request_workspace_close(state, app),
                b' ' => app.command_palette = Some(CommandPalette::default()),
                b'1'..=b'9' => select_workspace(state, app, usize::from(key - b'1')),
                PREFIX_KEY => {
                    if let Some(pane) = app.selected_pane.clone()
                        && app
                            .routes
                            .get(&pane)
                            .is_some_and(|route| route.access == TerminalAccess::Control)
                        && let Some(client) = app.client.clone()
                    {
                        client.send_input(pane, vec![PREFIX_KEY]);
                    }
                }
                0x1b => {}
                _ => {
                    app.message = Some(
                        "prefix: Space actions, 1-9 workspace, p/n tab, j/k pane, d close workspace, a agents, e events, h hosts, v paste, i image, P pair a device, q quit, Ctrl+] literal"
                            .into(),
                    );
                }
            }
            app.mode = InputMode::Terminal;
        }
        InputMode::HerdrPrefix => {
            handle_herdr_prefix(key, state, app)?;
            app.mode = InputMode::Terminal;
        }
    }
    Ok(false)
}

fn handle_herdr_prefix(key: u8, state: &FederationState, app: &mut App) -> Result<()> {
    match key {
        b'h' => select_neighbor_pane(state, app, PaneDirection::Left),
        b'j' => select_neighbor_pane(state, app, PaneDirection::Down),
        b'k' => select_neighbor_pane(state, app, PaneDirection::Up),
        b'l' => select_neighbor_pane(state, app, PaneDirection::Right),
        b'p' => cycle_tab(state, app, -1),
        b'n' => cycle_tab(state, app, 1),
        b'1'..=b'9' => select_tab_index(state, app, usize::from(key - b'1')),
        b'c' => {
            let Some((_, pane)) = selected_snapshot_and_pane(state, app.selected_pane.as_ref())
            else {
                app.message = Some("Herdr: no workspace is selected".to_owned());
                return Ok(());
            };
            let Some(workspace) = pane.workspace.as_ref() else {
                app.message = Some("Herdr: selected pane has no workspace".to_owned());
                return Ok(());
            };
            run_operation(
                app,
                Operation::CreateTab {
                    workspace: workspace.clone(),
                },
                "created tab".to_owned(),
                true,
            );
        }
        b'v' => run_selected_pane_operation(
            app,
            |pane| Operation::SplitPane {
                pane,
                direction: SplitDirection::Right,
            },
            "split pane vertically",
            true,
        ),
        b'-' => run_selected_pane_operation(
            app,
            |pane| Operation::SplitPane {
                pane,
                direction: SplitDirection::Down,
            },
            "split pane horizontally",
            true,
        ),
        b'z' => run_selected_pane_operation(
            app,
            |pane| Operation::TogglePaneZoom { pane },
            "toggled pane zoom",
            false,
        ),
        b'?' => {
            app.message = Some(
                "Herdr Ctrl+B: h/j/k/l pane, p/n tab, 1-9 tab, c new tab, v/- split, z zoom"
                    .to_owned(),
            );
        }
        0x1b => {}
        _ => {
            app.message = Some(
                "Herdr Ctrl+B chord is not exposed by Herdr's public API; press Ctrl+B ? for supported actions"
                    .to_owned(),
            );
        }
    }
    Ok(())
}

/// Submit one resolved operation to the daemon.
///
/// The frontend no longer builds a Herdr command line: it names what it wants
/// and the daemon resolves the target, chooses the client, and extracts the
/// server-local identifier. What stays here is the single-action guard and the
/// description to show when the result comes back, since a result carries only
/// a request identifier.
fn run_operation(
    app: &mut App,
    operation: Operation,
    description: String,
    follow_server_focus: bool,
) {
    if app.herdr_action_inflight {
        app.message = Some("Herdr action already in progress".to_owned());
        return;
    }
    let Some(client) = app.client.as_ref() else {
        app.message = Some("Herdr action routing is unavailable".to_owned());
        return;
    };
    let request = client.run_operation(operation);
    app.herdr_action_inflight = true;
    app.message = Some(format!("Herdr: {description}…"));
    app.pending_operations.insert(
        request,
        PendingOperation {
            description,
            follow_server_focus,
            clipboard_feedback: None,
        },
    );
}

/// Run an operation against the selected pane's own pane identifier.
fn run_selected_pane_operation(
    app: &mut App,
    build: impl FnOnce(PaneId) -> Operation,
    description: &str,
    follow_server_focus: bool,
) {
    let Some(pane) = app.selected_pane.clone() else {
        app.message = Some("Herdr: no pane is selected".to_owned());
        return;
    };
    run_operation(
        app,
        build(pane),
        description.to_owned(),
        follow_server_focus,
    );
}

struct WorkspaceMove {
    workspace: WorkspaceId,
    destination: WorkspaceId,
    label: String,
    destination_label: String,
}

/// Move one workspace into another on the same Herdr session. The whole
/// multi-request sequence runs in one task and reports one result, so the
/// single-action guard and per-target failure isolation still hold. Herdr
/// re-qualifies moved pane identifiers, so the selection follows server focus
/// once the move lands.
fn spawn_workspace_move(state: &FederationState, app: &mut App, move_request: WorkspaceMove) {
    let key = move_request.workspace.target_session();
    if key != move_request.destination.target_session() {
        app.message = Some("Herdr: a workspace can only move inside one session".to_owned());
        return;
    }
    if move_request.workspace == move_request.destination {
        app.message = Some("Herdr: a workspace cannot move into itself".to_owned());
        return;
    }
    if state
        .targets
        .get(&key)
        .is_none_or(|runtime| runtime.connection != TargetConnectionState::Live)
    {
        app.message = Some(format!("Herdr: {key} is not connected"));
        return;
    }
    let WorkspaceMove {
        workspace,
        destination,
        label,
        destination_label,
    } = move_request;
    run_operation(
        app,
        Operation::MoveWorkspace {
            workspace,
            destination,
        },
        format!("moved workspace {label:?} into {destination_label:?} on {key}"),
        true,
    );
}

/// Recreate a workspace on another session. Two Herdr servers are involved, so
/// both must be live; the source is only read, and nothing there is closed.
fn spawn_workspace_recreate(
    state: &FederationState,
    app: &mut App,
    workspace: WorkspaceId,
    destination: TargetSession,
    label: String,
) {
    let source_key = workspace.target_session();
    if source_key == destination {
        app.message = Some("Herdr: use a move inside one session".to_owned());
        return;
    }
    // Two Herdr servers are involved, so both must be live before anything is
    // built on the destination.
    for key in [&source_key, &destination] {
        if state
            .targets
            .get(key)
            .is_none_or(|runtime| runtime.connection != TargetConnectionState::Live)
        {
            app.message = Some(format!("Herdr: {key} is not connected"));
            return;
        }
    }
    let description =
        format!("recreated workspace {label:?} from {source_key} on {destination} (new shells)");
    run_operation(
        app,
        Operation::RecreateWorkspace {
            workspace,
            destination,
            label: (!label.is_empty()).then_some(label),
        },
        description,
        false,
    );
}

fn request_workspace_close(state: &FederationState, app: &mut App) {
    if app.herdr_action_inflight {
        app.message = Some("Herdr action already in progress".to_owned());
        return;
    }
    let Some((snapshot, pane)) = selected_snapshot_and_pane(state, app.selected_pane.as_ref())
    else {
        app.message = Some("Herdr: no workspace is selected".to_owned());
        return;
    };
    let Some(workspace_id) = pane.workspace.as_ref() else {
        app.message = Some("Herdr: selected pane has no workspace".to_owned());
        return;
    };
    let Some(workspace) = snapshot.workspaces.get(workspace_id) else {
        app.message = Some("Herdr: selected workspace is unavailable".to_owned());
        return;
    };
    app.close_confirmation = Some(CloseConfirmation {
        action: ResourceAction::CloseWorkspace {
            workspace: workspace.id.clone(),
            label: display_label(&workspace.id.resource, workspace.label.as_deref()),
        },
    });
    app.message = None;
}

fn command_palette_actions(
    state: &FederationState,
    selected: Option<&PaneId>,
) -> Vec<ResourceAction> {
    let mut actions = vec![
        ResourceAction::OpenTargetManager,
        ResourceAction::OpenAgentNavigator,
        ResourceAction::OpenAttentionCenter,
    ];

    let selected_is_actionable = selected.is_some_and(|pane| {
        state
            .targets
            .get(&pane.target_session())
            .is_some_and(|target| {
                target.connection == TargetConnectionState::Live
                    && target.selected_herdr_bin.is_some()
            })
    });
    if selected_is_actionable
        && let Some((snapshot, pane)) = selected_snapshot_and_pane(state, selected)
    {
        if let Some(workspace_id) = pane.workspace.as_ref()
            && let Some(workspace) = snapshot.workspaces.get(workspace_id)
        {
            let label = display_label(&workspace.id.resource, workspace.label.as_deref());
            actions.push(ResourceAction::RenameWorkspace {
                workspace: workspace.id.clone(),
                current_label: label.clone(),
            });
            actions.push(ResourceAction::CloseWorkspace {
                workspace: workspace.id.clone(),
                label,
            });
            actions.push(ResourceAction::CreateTab {
                workspace: workspace.id.clone(),
            });
            actions.extend(workspace_move_actions(snapshot, &workspace.id));
            actions.extend(workspace_recreate_actions(state, snapshot, &workspace.id));
        }
        if let Some(tab_id) = pane.tab.as_ref()
            && let Some(tab) = snapshot.tabs.get(tab_id)
        {
            let label = display_label(&tab.id.resource, tab.label.as_deref());
            actions.push(ResourceAction::RenameTab {
                tab: tab.id.clone(),
                current_label: label.clone(),
            });
            actions.push(ResourceAction::CloseTab {
                tab: tab.id.clone(),
                label,
            });
        }
        let pane_label = display_label(&pane.id.resource, pane.label.as_deref());
        actions.extend([
            ResourceAction::SplitPane {
                pane: pane.id.clone(),
                direction: SplitDirection::Right,
            },
            ResourceAction::SplitPane {
                pane: pane.id.clone(),
                direction: SplitDirection::Down,
            },
            ResourceAction::TogglePaneZoom {
                pane: pane.id.clone(),
            },
            ResourceAction::ClosePane {
                pane: pane.id.clone(),
                label: pane_label,
            },
        ]);
    }

    for runtime in state.targets.values() {
        if runtime.connection != TargetConnectionState::Live || runtime.selected_herdr_bin.is_none()
        {
            continue;
        }
        let Some(snapshot) = runtime.snapshot.as_deref() else {
            continue;
        };
        actions.push(ResourceAction::CreateWorkspace {
            target: runtime.key.clone(),
        });
        if let Some(pane) = snapshot
            .focused_pane
            .clone()
            .or_else(|| snapshot.panes.keys().next().cloned())
        {
            actions.push(ResourceAction::JumpToPane {
                pane,
                label: format!("session {}", runtime.key.session),
            });
        }
        for workspace in snapshot.workspaces.values() {
            let Some(pane) = pane_for_workspace(snapshot, &workspace.id) else {
                continue;
            };
            actions.push(ResourceAction::JumpToPane {
                pane,
                label: format!(
                    "workspace {}",
                    display_label(&workspace.id.resource, workspace.label.as_deref())
                ),
            });
        }
        for tab in snapshot.tabs.values() {
            let Some(pane) = pane_for_tab(snapshot, &tab.id) else {
                continue;
            };
            actions.push(ResourceAction::JumpToPane {
                pane,
                label: format!(
                    "tab {}",
                    display_label(&tab.id.resource, tab.label.as_deref())
                ),
            });
        }
        for pane in snapshot.panes.values() {
            actions.push(ResourceAction::JumpToPane {
                pane: pane.id.clone(),
                label: format!(
                    "pane {}",
                    display_label(&pane.id.resource, pane.label.as_deref())
                ),
            });
        }
        for agent in snapshot.agents.values() {
            if !snapshot.panes.contains_key(&agent.pane) {
                continue;
            }
            let name = agent
                .name
                .as_deref()
                .or(agent.agent.as_deref())
                .unwrap_or("agent");
            actions.push(ResourceAction::JumpToPane {
                pane: agent.pane.clone(),
                label: format!("agent {}", safe_text(name)),
            });
        }
    }
    actions
}

/// A context menu is not scrollable, so it lists at most this many move
/// destinations in workspace-identifier order. The action palette lists every
/// destination and is searchable.
const MAX_CONTEXT_MENU_MOVE_DESTINATIONS: usize = 8;

/// Offer a move into every other workspace of the same session. Herdr relocates
/// live panes inside one server process, so a destination in another session is
/// never listed as a move.
fn workspace_move_actions(
    snapshot: &NormalizedSnapshot,
    source: &WorkspaceId,
) -> Vec<ResourceAction> {
    let Some(workspace) = snapshot.workspaces.get(source) else {
        return Vec::new();
    };
    let label = display_label(&workspace.id.resource, workspace.label.as_deref());
    snapshot
        .workspaces
        .values()
        .filter(|destination| &destination.id != source)
        .map(|destination| ResourceAction::MoveWorkspace {
            workspace: source.clone(),
            destination: destination.id.clone(),
            label: label.clone(),
            destination_label: display_label(
                &destination.id.resource,
                destination.label.as_deref(),
            ),
        })
        .collect()
}

/// Offer a recreation on every other live session, including sessions on other
/// hosts. Live panes cannot cross a session boundary, so the destination is a
/// session rather than an existing workspace and the action says that it starts
/// new shells.
fn workspace_recreate_actions(
    state: &FederationState,
    snapshot: &NormalizedSnapshot,
    source: &WorkspaceId,
) -> Vec<ResourceAction> {
    let Some(workspace) = snapshot.workspaces.get(source) else {
        return Vec::new();
    };
    let label = display_label(&workspace.id.resource, workspace.label.as_deref());
    let session = source.target_session();
    state
        .targets
        .values()
        .filter(|runtime| {
            runtime.key != session
                && runtime.connection == TargetConnectionState::Live
                && runtime.snapshot.is_some()
        })
        .map(|runtime| ResourceAction::RecreateWorkspace {
            workspace: source.clone(),
            destination: runtime.key.clone(),
            label: label.clone(),
        })
        .collect()
}

fn context_menu_for_target(
    state: &FederationState,
    target: ContextTarget,
    anchor: Position,
) -> Option<ContextMenu> {
    let (title, actions) = match target {
        ContextTarget::Session(target) => {
            actionable_snapshot(state, &target)?;
            (
                format!("session {target}"),
                vec![ResourceAction::CreateWorkspace { target }],
            )
        }
        ContextTarget::Workspace(workspace) => {
            let snapshot = actionable_snapshot(state, &workspace.target_session())?;
            let resource = snapshot.workspaces.get(&workspace)?;
            let label = display_label(&resource.id.resource, resource.label.as_deref());
            let mut actions = vec![
                ResourceAction::CreateTab {
                    workspace: workspace.clone(),
                },
                ResourceAction::RenameWorkspace {
                    workspace: workspace.clone(),
                    current_label: label.clone(),
                },
            ];
            actions.extend(
                workspace_move_actions(snapshot, &workspace)
                    .into_iter()
                    .take(MAX_CONTEXT_MENU_MOVE_DESTINATIONS),
            );
            actions.extend(
                workspace_recreate_actions(state, snapshot, &workspace)
                    .into_iter()
                    .take(MAX_CONTEXT_MENU_MOVE_DESTINATIONS),
            );
            let title = format!("workspace {label} · {}", workspace.target_session());
            actions.push(ResourceAction::CloseWorkspace { workspace, label });
            (title, actions)
        }
        ContextTarget::Tab(tab) => {
            let snapshot = actionable_snapshot(state, &tab.target_session())?;
            let resource = snapshot.tabs.get(&tab)?;
            let label = display_label(&resource.id.resource, resource.label.as_deref());
            (
                format!("tab {label} · {}", tab.target_session()),
                vec![
                    ResourceAction::RenameTab {
                        tab: tab.clone(),
                        current_label: label.clone(),
                    },
                    ResourceAction::CloseTab { tab, label },
                ],
            )
        }
        ContextTarget::Pane(pane) => {
            let snapshot = actionable_snapshot(state, &pane.target_session())?;
            let resource = snapshot.panes.get(&pane)?;
            let label = display_label(&resource.id.resource, resource.label.as_deref());
            (
                format!("pane {label} · {}", pane.target_session()),
                vec![
                    ResourceAction::SplitPane {
                        pane: pane.clone(),
                        direction: SplitDirection::Right,
                    },
                    ResourceAction::SplitPane {
                        pane: pane.clone(),
                        direction: SplitDirection::Down,
                    },
                    ResourceAction::TogglePaneZoom { pane: pane.clone() },
                    ResourceAction::ClosePane { pane, label },
                ],
            )
        }
    };
    Some(ContextMenu {
        title,
        actions,
        selected: 0,
        anchor,
        pressed: None,
    })
}

fn actionable_snapshot<'a>(
    state: &'a FederationState,
    target: &TargetSession,
) -> Option<&'a NormalizedSnapshot> {
    let runtime = state.targets.get(target)?;
    (runtime.connection == TargetConnectionState::Live && runtime.selected_herdr_bin.is_some())
        .then_some(runtime.snapshot.as_deref())
        .flatten()
}

fn open_context_menu(
    state: &FederationState,
    target: ContextTarget,
    anchor: Position,
    app: &mut App,
) {
    app.selection = None;
    app.selection_autoscroll = None;
    if let Some(menu) = context_menu_for_target(state, target, anchor) {
        app.context_menu = Some(menu);
        app.message = None;
    } else {
        app.message = Some("that resource is not currently actionable".to_owned());
    }
}

fn fuzzy_score(candidate: &str, query: &str) -> Option<usize> {
    let candidate = candidate.to_lowercase().chars().collect::<Vec<_>>();
    let query = query.trim().to_lowercase().chars().collect::<Vec<_>>();
    if query.is_empty() {
        return Some(0);
    }
    let mut cursor = 0;
    let mut score = 0;
    let mut previous = None;
    for expected in query {
        let position = candidate
            .iter()
            .enumerate()
            .skip(cursor)
            .find(|(_, character)| **character == expected)
            .map(|(position, _)| position)?;
        score += position.saturating_sub(cursor);
        if previous.is_some_and(|previous| position != previous + 1) {
            score += 2;
        }
        previous = Some(position);
        cursor = position + 1;
    }
    Some(score + candidate.len().saturating_sub(cursor) / 8)
}

fn filtered_palette_actions(
    state: &FederationState,
    selected: Option<&PaneId>,
    query: &str,
) -> Vec<ResourceAction> {
    let mut actions = command_palette_actions(state, selected)
        .into_iter()
        .filter_map(|action| fuzzy_score(&action.search_text(), query).map(|score| (score, action)))
        .collect::<Vec<_>>();
    actions.sort_by(|(left_score, left), (right_score, right)| {
        left_score
            .cmp(right_score)
            .then_with(|| left.palette_label().cmp(&right.palette_label()))
            .then_with(|| left.palette_scope().cmp(&right.palette_scope()))
    });
    actions.into_iter().map(|(_, action)| action).collect()
}

fn handle_command_palette_input(key: u8, state: &FederationState, app: &mut App) -> Result<bool> {
    let Some(mut palette) = app.command_palette.take() else {
        return Ok(false);
    };
    let actions = filtered_palette_actions(state, app.selected_pane.as_ref(), &palette.query);
    match key {
        0x1b => return Ok(false),
        b'\r' | b'\n' => {
            if let Some(action) = actions.get(palette.selected).cloned() {
                execute_resource_action(action, state, app);
                return Ok(false);
            }
        }
        b'\t' | 0x0e => {
            if !actions.is_empty() {
                palette.selected = (palette.selected + 1) % actions.len();
            }
        }
        0x10 => {
            if !actions.is_empty() {
                palette.selected = palette.selected.checked_sub(1).unwrap_or(actions.len() - 1);
            }
        }
        0x08 | 0x7f => {
            palette.query.pop();
            palette.selected = 0;
        }
        0x15 => {
            palette.query.clear();
            palette.selected = 0;
        }
        0x20..=0x7e => {
            palette.query.push(char::from(key));
            palette.selected = 0;
        }
        _ => {}
    }
    let count = filtered_palette_actions(state, app.selected_pane.as_ref(), &palette.query).len();
    palette.selected = palette.selected.min(count.saturating_sub(1));
    app.command_palette = Some(palette);
    Ok(false)
}

fn handle_context_menu_input(key: u8, state: &FederationState, app: &mut App) -> Result<bool> {
    let Some(mut menu) = app.context_menu.take() else {
        return Ok(false);
    };
    match key {
        b'q' | 0x1b => return Ok(false),
        b'j' | b'\t' | 0x0e => {
            if !menu.actions.is_empty() {
                menu.selected = (menu.selected + 1) % menu.actions.len();
            }
        }
        b'k' | 0x10 => {
            if !menu.actions.is_empty() {
                menu.selected = menu
                    .selected
                    .checked_sub(1)
                    .unwrap_or(menu.actions.len() - 1);
            }
        }
        b'\r' | b'\n' => {
            if let Some(action) = menu.actions.get(menu.selected).cloned() {
                execute_resource_action(action, state, app);
            }
            return Ok(false);
        }
        _ => {}
    }
    app.context_menu = Some(menu);
    Ok(false)
}

fn handle_context_menu_mouse(mouse: MouseInput, state: &FederationState, app: &mut App) {
    let Some(frame_area) = app.last_frame_area else {
        app.context_menu = None;
        return;
    };
    let Some(menu) = app.context_menu.as_ref() else {
        return;
    };
    let outer = Position::new(mouse.column.saturating_sub(1), mouse.row.saturating_sub(1));
    let action_at_pointer = context_menu_action_at(frame_area, menu, outer);

    if let Some(direction) = mouse.scroll_direction() {
        let menu = app
            .context_menu
            .as_mut()
            .expect("context menu was checked above");
        if !menu.actions.is_empty() {
            menu.selected = match direction {
                TerminalScrollDirection::Up => menu.selected.saturating_sub(1),
                TerminalScrollDirection::Down => (menu.selected + 1).min(menu.actions.len() - 1),
            };
        }
        return;
    }
    if mouse.is_left_press() {
        if let Some(index) = action_at_pointer {
            let menu = app
                .context_menu
                .as_mut()
                .expect("context menu was checked above");
            menu.selected = index;
            menu.pressed = Some(index);
        } else {
            app.context_menu = None;
        }
        return;
    }
    if mouse.is_left_release() {
        let action = app.context_menu.as_ref().and_then(|menu| {
            (menu.pressed == action_at_pointer)
                .then_some(action_at_pointer)
                .flatten()
                .and_then(|index| menu.actions.get(index).cloned())
        });
        if let Some(action) = action {
            app.context_menu = None;
            execute_resource_action(action, state, app);
        } else if let Some(menu) = app.context_menu.as_mut() {
            menu.pressed = None;
        }
    } else if mouse.is_right_press() {
        app.context_menu = None;
    }
}

fn execute_resource_action(action: ResourceAction, state: &FederationState, app: &mut App) {
    if action.mutates_herdr() && app.herdr_action_inflight {
        app.message = Some("Herdr action already in progress".to_owned());
        return;
    }
    match action {
        ResourceAction::JumpToPane { pane, .. } => select_pane(app, pane),
        ResourceAction::OpenTargetManager => {
            app.target_manager = Some(TargetManager::new(app.configured_targets.clone()));
        }
        ResourceAction::OpenAgentNavigator => {
            app.agent_navigator = Some(AgentNavigator::default());
        }
        ResourceAction::OpenAttentionCenter => {
            app.attention_center = Some(AttentionCenter::default());
        }
        ResourceAction::CreateWorkspace { target } => {
            app.text_prompt = Some(TextPrompt {
                title: "create Herdr workspace".to_owned(),
                label: format!("Workspace label on {target}"),
                value: String::new(),
                action: TextPromptAction::CreateWorkspace { target },
                error: None,
            });
        }
        ResourceAction::RenameWorkspace {
            workspace,
            current_label,
        } => {
            app.text_prompt = Some(TextPrompt {
                title: "rename Herdr workspace".to_owned(),
                label: format!("New label for {current_label:?}"),
                value: String::new(),
                action: TextPromptAction::RenameWorkspace { workspace },
                error: None,
            });
        }
        ResourceAction::RenameTab { tab, current_label } => {
            app.text_prompt = Some(TextPrompt {
                title: "rename Herdr tab".to_owned(),
                label: format!("New label for {current_label:?}"),
                value: String::new(),
                action: TextPromptAction::RenameTab { tab },
                error: None,
            });
        }
        ResourceAction::CloseWorkspace { .. }
        | ResourceAction::CloseTab { .. }
        | ResourceAction::ClosePane { .. } => {
            app.close_confirmation = Some(CloseConfirmation { action });
        }
        ResourceAction::CreateTab { workspace } => {
            let target_session = workspace.target_session();
            run_operation(
                app,
                Operation::CreateTab { workspace },
                format!("created tab on {target_session}"),
                true,
            );
        }
        ResourceAction::MoveWorkspace {
            workspace,
            destination,
            label,
            destination_label,
        } => {
            spawn_workspace_move(
                state,
                app,
                WorkspaceMove {
                    workspace,
                    destination,
                    label,
                    destination_label,
                },
            );
        }
        ResourceAction::RecreateWorkspace {
            workspace,
            destination,
            label,
        } => {
            spawn_workspace_recreate(state, app, workspace, destination, label);
        }
        ResourceAction::SplitPane { pane, direction } => {
            let target_session = pane.target_session();
            run_operation(
                app,
                Operation::SplitPane { pane, direction },
                format!("split pane {} on {target_session}", direction.label()),
                true,
            );
        }
        ResourceAction::TogglePaneZoom { pane } => {
            let target_session = pane.target_session();
            run_operation(
                app,
                Operation::TogglePaneZoom { pane },
                format!("toggled pane zoom on {target_session}"),
                false,
            );
        }
    }
}

fn handle_text_prompt_input(key: u8, app: &mut App) -> Result<bool> {
    let Some(mut prompt) = app.text_prompt.take() else {
        return Ok(false);
    };
    match key {
        0x1b => return Ok(false),
        b'\r' | b'\n' => {
            let value = prompt.value.trim().to_owned();
            if value.is_empty() {
                prompt.error = Some("A non-empty label is required".to_owned());
            } else {
                // The prompt is where an intent becomes an operation: the label
                // the person typed is now part of what the daemon is asked to do.
                let (operation, description, follow_server_focus) = match prompt.action {
                    TextPromptAction::CreateWorkspace { target } => (
                        Operation::CreateWorkspace {
                            target: target.clone(),
                            label: value.clone(),
                        },
                        format!("created workspace {value:?} on {target}"),
                        true,
                    ),
                    TextPromptAction::RenameWorkspace { workspace } => {
                        let target = workspace.target_session();
                        (
                            Operation::RenameWorkspace {
                                workspace,
                                label: value.clone(),
                            },
                            format!("renamed workspace to {value:?} on {target}"),
                            false,
                        )
                    }
                    TextPromptAction::RenameTab { tab } => {
                        let target = tab.target_session();
                        (
                            Operation::RenameTab {
                                tab,
                                label: value.clone(),
                            },
                            format!("renamed tab to {value:?} on {target}"),
                            false,
                        )
                    }
                };
                run_operation(app, operation, description, follow_server_focus);
                return Ok(false);
            }
        }
        0x08 | 0x7f => {
            prompt.value.pop();
            prompt.error = None;
        }
        0x15 => {
            prompt.value.clear();
            prompt.error = None;
        }
        0x20..=0x7e => {
            prompt.value.push(char::from(key));
            prompt.error = None;
        }
        _ => {}
    }
    app.text_prompt = Some(prompt);
    Ok(false)
}

fn handle_close_confirmation_input(key: u8, app: &mut App) -> Result<bool> {
    let Some(confirmation) = app.close_confirmation.take() else {
        return Ok(false);
    };
    match key {
        b'y' => {
            // Confirmation is what turns a destructive intent into an
            // operation. Nothing that has not been agreed to reaches the daemon.
            let (operation, description) = match confirmation.action {
                ResourceAction::CloseWorkspace { workspace, label } => {
                    let target = workspace.target_session();
                    (
                        Operation::CloseWorkspace { workspace },
                        format!("closed workspace {label:?} on {target}"),
                    )
                }
                ResourceAction::CloseTab { tab, label } => {
                    let target = tab.target_session();
                    (
                        Operation::CloseTab { tab },
                        format!("closed tab {label:?} on {target}"),
                    )
                }
                ResourceAction::ClosePane { pane, label } => {
                    let target = pane.target_session();
                    (
                        Operation::ClosePane { pane },
                        format!("closed pane {label:?} on {target}"),
                    )
                }
                action => {
                    app.message = Some(format!(
                        "refused non-destructive action {:?} in close confirmation",
                        action.palette_label()
                    ));
                    return Ok(false);
                }
            };
            run_operation(app, operation, description, true);
        }
        b'n' | b'q' | 0x1b => {}
        _ => app.close_confirmation = Some(confirmation),
    }
    Ok(false)
}

fn handle_target_manager_mouse(
    mouse: MouseInput,
    frame_area: Rect,
    transport_config: &TransportConfig,
    app: &mut App,
) -> Result<()> {
    let outer = Position::new(mouse.column.saturating_sub(1), mouse.row.saturating_sub(1));
    if let Some(direction) = mouse.scroll_direction() {
        let Some(manager) = app.target_manager.as_mut() else {
            return Ok(());
        };
        match &mut manager.mode {
            TargetManagerMode::List => {
                if manager.targets.is_empty() {
                    return Ok(());
                }
                manager.selected = match direction {
                    TerminalScrollDirection::Up => manager.selected.saturating_sub(1),
                    TerminalScrollDirection::Down => {
                        (manager.selected + 1).min(manager.targets.len() - 1)
                    }
                };
            }
            TargetManagerMode::Form(form) if !form.discovered_sessions.is_empty() => {
                form.field = TargetFormField::DiscoveredSession;
                form.discovered_selected = match direction {
                    TerminalScrollDirection::Up => form.discovered_selected.saturating_sub(1),
                    TerminalScrollDirection::Down => {
                        (form.discovered_selected + 1).min(form.discovered_sessions.len() - 1)
                    }
                };
            }
            TargetManagerMode::Form(_) | TargetManagerMode::ConfirmRemove { .. } => {}
        }
        return Ok(());
    }

    let control = app.target_manager.as_ref().and_then(|manager| {
        target_manager_controls(frame_area, manager)
            .into_iter()
            .find_map(|(area, control)| area.contains(outer).then_some(control))
    });
    if mouse.is_left_press() {
        if let Some(manager) = app.target_manager.as_mut() {
            manager.mouse_press = control;
        }
        return Ok(());
    }
    if !mouse.is_left_release() {
        return Ok(());
    }
    let pressed = app
        .target_manager
        .as_mut()
        .and_then(|manager| manager.mouse_press.take());
    if pressed.is_none() || pressed != control {
        return Ok(());
    }
    let Some(control) = control else {
        return Ok(());
    };
    let Some(mut manager) = app.target_manager.take() else {
        return Ok(());
    };
    let close_manager =
        activate_target_manager_control(control, transport_config, &mut manager, app)?;
    if !close_manager {
        app.target_manager = Some(manager);
    }
    Ok(())
}

fn handle_target_manager_input(
    key: u8,
    transport_config: &TransportConfig,
    app: &mut App,
) -> Result<bool> {
    let Some(mut manager) = app.target_manager.take() else {
        return Ok(false);
    };
    let mut close_manager = false;
    match &mut manager.mode {
        TargetManagerMode::List => match key {
            b'q' | 0x1b => close_manager = true,
            b'j' | 0x0e => {
                if !manager.targets.is_empty() {
                    manager.selected = (manager.selected + 1).min(manager.targets.len() - 1);
                }
            }
            b'k' | 0x10 => manager.selected = manager.selected.saturating_sub(1),
            b'a' => {
                close_manager |= activate_target_manager_control(
                    TargetManagerControl::Add,
                    transport_config,
                    &mut manager,
                    app,
                )?;
            }
            b'e' | b'\r' | b'\n' => {
                close_manager |= activate_target_manager_control(
                    TargetManagerControl::Edit,
                    transport_config,
                    &mut manager,
                    app,
                )?;
            }
            b'd' => {
                close_manager |= activate_target_manager_control(
                    TargetManagerControl::Remove,
                    transport_config,
                    &mut manager,
                    app,
                )?;
            }
            _ => {}
        },
        TargetManagerMode::Form(form) => match key {
            0x1b => manager.mode = TargetManagerMode::List,
            b'\t' => {
                form.field = form.field.next(!form.discovered_sessions.is_empty());
                form.error = None;
            }
            0x10 if form.field == TargetFormField::DiscoveredSession => {
                form.discovered_selected = form.discovered_selected.saturating_sub(1);
            }
            0x0e if form.field == TargetFormField::DiscoveredSession => {
                form.discovered_selected = (form.discovered_selected + 1)
                    .min(form.discovered_sessions.len().saturating_sub(1));
            }
            0x14 => start_target_connection_test(&mut manager, transport_config, app),
            b'\r' | b'\n' => {
                if form.field == TargetFormField::DiscoveredSession {
                    form.select_discovered_session(form.discovered_selected);
                } else if form.field == TargetFormField::DiscoverSessions {
                    form.invalidate_test();
                    form.discover_sessions = !form.discover_sessions;
                    form.field = TargetFormField::DiscoverSessions;
                } else {
                    save_target_form(&mut manager, app)?;
                }
            }
            0x08 | 0x7f => {
                if matches!(
                    form.field,
                    TargetFormField::Name | TargetFormField::Ssh | TargetFormField::Session
                ) {
                    if form.field == TargetFormField::Session {
                        form.socket = None;
                    }
                    form.invalidate_test();
                    if let Some(text) = form.active_text_mut() {
                        text.pop();
                    }
                }
            }
            b' ' if form.field == TargetFormField::DiscoverSessions => {
                form.invalidate_test();
                form.discover_sessions = !form.discover_sessions;
                form.field = TargetFormField::DiscoverSessions;
            }
            0x20..=0x7e => {
                if matches!(
                    form.field,
                    TargetFormField::Name | TargetFormField::Ssh | TargetFormField::Session
                ) {
                    if form.field == TargetFormField::Session {
                        form.socket = None;
                    }
                    form.invalidate_test();
                    if let Some(text) = form.active_text_mut() {
                        text.push(char::from(key));
                    }
                }
            }
            _ => {}
        },
        TargetManagerMode::ConfirmRemove { name: _ } => match key {
            b'y' => remove_target(&mut manager, app)?,
            b'n' | b'q' | 0x1b => manager.mode = TargetManagerMode::List,
            _ => {}
        },
    }
    if !close_manager {
        app.target_manager = Some(manager);
    }
    Ok(false)
}

fn activate_target_manager_control(
    control: TargetManagerControl,
    transport_config: &TransportConfig,
    manager: &mut TargetManager,
    app: &mut App,
) -> Result<bool> {
    let mut close_manager = false;
    match control {
        TargetManagerControl::Target(index) => {
            if index < manager.targets.len() {
                manager.selected = index;
            }
        }
        TargetManagerControl::Add => {
            manager.mode = TargetManagerMode::Form(Box::new(TargetForm::add()))
        }
        TargetManagerControl::Edit => {
            if let Some(target) = manager.selected_target() {
                manager.mode = TargetManagerMode::Form(Box::new(TargetForm::edit(target)));
            }
        }
        TargetManagerControl::Remove => {
            if let Some(target) = manager.selected_target() {
                manager.mode = TargetManagerMode::ConfirmRemove {
                    name: target.name.clone(),
                };
            }
        }
        TargetManagerControl::Close => close_manager = true,
        TargetManagerControl::Field(field) => {
            if let TargetManagerMode::Form(form) = &mut manager.mode {
                if field == TargetFormField::DiscoverSessions {
                    form.invalidate_test();
                    form.discover_sessions = !form.discover_sessions;
                }
                form.field = field;
                form.error = None;
            }
        }
        TargetManagerControl::TestConnection => {
            start_target_connection_test(manager, transport_config, app)
        }
        TargetManagerControl::Save => save_target_form(manager, app)?,
        TargetManagerControl::Cancel | TargetManagerControl::CancelRemove => {
            manager.mode = TargetManagerMode::List
        }
        TargetManagerControl::DiscoveredSession(index) => {
            if let TargetManagerMode::Form(form) = &mut manager.mode {
                form.select_discovered_session(index);
            }
        }
        TargetManagerControl::ConfirmRemove => remove_target(manager, app)?,
    }
    Ok(close_manager)
}

fn save_target_form(manager: &mut TargetManager, app: &mut App) -> Result<()> {
    let TargetManagerMode::Form(form) = &manager.mode else {
        return Ok(());
    };
    if let Some(error) = form.validation_error(&manager.targets) {
        if let TargetManagerMode::Form(form) = &mut manager.mode {
            form.error = Some(error);
        }
        return Ok(());
    }
    let Some(path) = app.config_path.clone() else {
        if let TargetManagerMode::Form(form) = &mut manager.mode {
            form.error = Some("configuration path is unavailable".to_owned());
        }
        return Ok(());
    };
    let (target, original_name, updated_name) = match &manager.mode {
        TargetManagerMode::Form(form) => {
            (form.target(), form.original_name.clone(), form.name.clone())
        }
        _ => return Ok(()),
    };
    let result = match original_name.as_deref() {
        Some(name) => Config::replace_target_file(Some(&path), name, target),
        None => Config::add_target_file(Some(&path), target),
    };
    match result {
        Ok(_) => {
            let (config, _) = Config::load(Some(&path))?;
            manager.targets = config.targets.clone();
            manager.selected = manager
                .targets
                .iter()
                .position(|target| target.name == updated_name)
                .unwrap_or_default();
            manager.mode = TargetManagerMode::List;
            app.configured_targets = config.targets;
            app.configuration_dirty = true;
            app.message = Some(format!(
                "saved target {updated_name:?}; refreshing federation"
            ));
        }
        Err(error) => {
            if let TargetManagerMode::Form(form) = &mut manager.mode {
                form.error = Some(error.to_string());
            }
        }
    }
    Ok(())
}

fn remove_target(manager: &mut TargetManager, app: &mut App) -> Result<()> {
    let TargetManagerMode::ConfirmRemove { name } = &manager.mode else {
        return Ok(());
    };
    let Some(path) = app.config_path.clone() else {
        app.message = Some("configuration path is unavailable".to_owned());
        manager.mode = TargetManagerMode::List;
        return Ok(());
    };
    let removed_name = name.clone();
    match Config::remove_target_file(Some(&path), &removed_name) {
        Ok(_) => {
            let (config, _) = Config::load(Some(&path))?;
            manager.targets = config.targets.clone();
            manager.selected = manager
                .selected
                .min(manager.targets.len().saturating_sub(1));
            manager.mode = TargetManagerMode::List;
            app.configured_targets = config.targets;
            app.configuration_dirty = true;
            app.message = Some(format!(
                "removed target {removed_name:?}; no Herdr session was touched"
            ));
        }
        Err(error) => {
            app.message = Some(error.to_string());
            manager.mode = TargetManagerMode::List;
        }
    }
    Ok(())
}

fn start_target_connection_test(
    manager: &mut TargetManager,
    transport_config: &TransportConfig,
    app: &mut App,
) {
    let TargetManagerMode::Form(form) = &manager.mode else {
        return;
    };
    if let Some(error) = form.validation_error(&manager.targets) {
        if let TargetManagerMode::Form(form) = &mut manager.mode {
            form.error = Some(error);
        }
        return;
    }
    let Some(sender) = app.target_test_sender.clone() else {
        if let TargetManagerMode::Form(form) = &mut manager.mode {
            form.error = Some("connection testing is unavailable".to_owned());
        }
        return;
    };
    let target = form.target();
    let transport = transport_config.clone();
    let request_id = app.next_target_test_request;
    app.next_target_test_request = app.next_target_test_request.saturating_add(1);
    if let TargetManagerMode::Form(form) = &mut manager.mode {
        form.error = None;
        form.test_state = TargetTestState::Testing { request_id };
        form.discovered_sessions.clear();
        form.discovered_selected = 0;
        form.discovered_offset = 0;
    }
    tokio::spawn(async move {
        let started = Instant::now();
        let result = discover_running_sessions(&target, &transport)
            .await
            .map_err(|error| error.to_string());
        let _ = sender.send(TargetTestEvent {
            request_id,
            elapsed_ms: started.elapsed().as_millis(),
            result,
        });
    });
}

fn apply_target_test_event(event: TargetTestEvent, app: &mut App) {
    let Some(manager) = app.target_manager.as_mut() else {
        return;
    };
    let TargetManagerMode::Form(form) = &mut manager.mode else {
        return;
    };
    if !matches!(
        form.test_state,
        TargetTestState::Testing { request_id } if request_id == event.request_id
    ) {
        return;
    }
    match event.result {
        Ok(discovery) => {
            let count = discovery.sessions.len();
            form.discovered_selected = form
                .session
                .is_empty()
                .then_some(0)
                .or_else(|| {
                    discovery
                        .sessions
                        .iter()
                        .position(|session| session.name == form.session)
                })
                .unwrap_or_default();
            form.discovered_sessions = discovery.sessions;
            form.discovered_offset = 0;
            form.test_state = TargetTestState::Passed {
                summary: if count == 0 {
                    format!(
                        "Connected in {} ms; no running sessions found",
                        event.elapsed_ms
                    )
                } else {
                    format!(
                        "Connected in {} ms; found {count} running session{}",
                        event.elapsed_ms,
                        if count == 1 { "" } else { "s" }
                    )
                },
            };
        }
        Err(message) => {
            form.discovered_sessions.clear();
            form.test_state = TargetTestState::Failed { message };
        }
    }
}

fn handle_agent_navigator_input(key: u8, state: &FederationState, app: &mut App) -> Result<bool> {
    let Some(mut navigator) = app.agent_navigator.take() else {
        return Ok(false);
    };
    let entries = agent_jump_entries(state, navigator.filter);
    match key {
        b'q' | 0x1b => return Ok(false),
        b'j' => {
            if !entries.is_empty() {
                navigator.selected = (navigator.selected + 1).min(entries.len() - 1);
            }
        }
        b'k' => navigator.selected = navigator.selected.saturating_sub(1),
        b'f' => {
            navigator.filter = navigator.filter.next();
            navigator.selected = 0;
        }
        b'\r' | b'\n' => {
            if let Some(entry) = entries.get(navigator.selected) {
                select_pane(app, entry.pane.clone());
                app.message = Some(format!(
                    "jumped to {} on {}/{}",
                    entry.agent, entry.pane.target, entry.pane.session
                ));
            }
            return Ok(false);
        }
        _ => {}
    }
    navigator.selected = navigator.selected.min(entries.len().saturating_sub(1));
    app.agent_navigator = Some(navigator);
    Ok(false)
}

fn handle_attention_center_input(key: u8, state: &FederationState, app: &mut App) -> Result<bool> {
    let Some(mut center) = app.attention_center.take() else {
        return Ok(false);
    };
    let event_count = app.attention.events().count();
    match key {
        b'q' | 0x1b => return Ok(false),
        b'j' => {
            if event_count > 0 {
                center.selected = (center.selected + 1).min(event_count - 1);
            }
        }
        b'k' => center.selected = center.selected.saturating_sub(1),
        b'r' => {
            if app.attention.mark_all_seen()
                && let Some(client) = app.client.as_ref()
            {
                client.mark_all_attention_seen();
            }
        }
        b'c' => {
            if app.attention.clear_seen()
                && let Some(client) = app.client.as_ref()
            {
                client.clear_seen_attention();
            }
            center.selected = center
                .selected
                .min(app.attention.events().count().saturating_sub(1));
        }
        b'\r' | b'\n' => {
            let event = app.attention.events().rev().nth(center.selected).cloned();
            if let Some(event) = event {
                if pane_is_live(state, &event.pane) {
                    select_pane(app, event.pane.clone());
                    app.message = Some(format!(
                        "jumped to {} on {}/{}",
                        safe_text(&event.agent),
                        event.pane.target,
                        event.pane.session
                    ));
                    return Ok(false);
                }
                mark_attention_seen(app, &event.pane);
                app.message = Some(format!(
                    "{} on {}/{} is no longer live",
                    safe_text(&event.agent),
                    event.pane.target,
                    event.pane.session
                ));
                return Ok(false);
            }
        }
        _ => {}
    }
    center.selected = center
        .selected
        .min(app.attention.events().count().saturating_sub(1));
    app.attention_center = Some(center);
    Ok(false)
}

fn pane_is_live(state: &FederationState, pane: &PaneId) -> bool {
    state
        .targets
        .get(&pane.target_session())
        .is_some_and(|target| {
            target.connection == TargetConnectionState::Live
                && target
                    .snapshot
                    .as_deref()
                    .is_some_and(|snapshot| snapshot.panes.contains_key(pane))
        })
}

fn agent_jump_entries(state: &FederationState, filter: AgentFilter) -> Vec<AgentJumpEntry> {
    let mut entries = state
        .targets
        .values()
        .filter(|target| target.connection == TargetConnectionState::Live)
        .filter_map(|target| target.snapshot.as_deref())
        .flat_map(|snapshot| {
            snapshot.agents.values().filter_map(move |agent| {
                let pane = snapshot.panes.get(&agent.pane);
                let status = agent
                    .status
                    .as_deref()
                    .or_else(|| pane.and_then(|pane| pane.agent_status.as_deref()))
                    .unwrap_or("unknown");
                let interactive_ready = agent.interactive_ready.unwrap_or(false);
                let include = match filter {
                    AgentFilter::All => true,
                    AgentFilter::Attention => agent_needs_attention(status, interactive_ready),
                    AgentFilter::Active => {
                        agent_needs_attention(status, interactive_ready) || agent_is_working(status)
                    }
                };
                include.then(|| {
                    let workspace = pane
                        .and_then(|pane| pane.workspace.as_ref())
                        .and_then(|workspace| snapshot.workspaces.get(workspace))
                        .map(|workspace| {
                            display_label(&workspace.id.resource, workspace.label.as_deref())
                        })
                        .unwrap_or_else(|| "unassigned".to_owned());
                    AgentJumpEntry {
                        pane: agent.pane.clone(),
                        agent: agent
                            .name
                            .as_deref()
                            .or(agent.agent.as_deref())
                            .or_else(|| pane.and_then(|pane| pane.agent.as_deref()))
                            .map(safe_text)
                            .unwrap_or_else(|| safe_text(&agent.pane.resource)),
                        workspace,
                        status: safe_text(status),
                        interactive_ready,
                    }
                })
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        agent_priority(&left.status, left.interactive_ready)
            .cmp(&agent_priority(&right.status, right.interactive_ready))
            .then_with(|| left.pane.target.cmp(&right.pane.target))
            .then_with(|| left.pane.session.cmp(&right.pane.session))
            .then_with(|| left.workspace.cmp(&right.workspace))
            .then_with(|| left.agent.cmp(&right.agent))
    });
    entries
}

fn agent_needs_attention(status: &str, interactive_ready: bool) -> bool {
    interactive_ready
        || matches!(
            status.to_ascii_lowercase().as_str(),
            "blocked" | "waiting" | "waiting_for_input" | "needs_input" | "ready"
        )
}

fn agent_is_working(status: &str) -> bool {
    matches!(
        status.to_ascii_lowercase().as_str(),
        "working" | "running" | "busy" | "active"
    )
}

fn agent_priority(status: &str, interactive_ready: bool) -> u8 {
    if agent_needs_attention(status, interactive_ready) {
        0
    } else if agent_is_working(status) {
        1
    } else if matches!(
        status.to_ascii_lowercase().as_str(),
        "idle" | "completed" | "done"
    ) {
        3
    } else {
        2
    }
}

async fn paste_system_clipboard(state: &FederationState, app: &mut App) -> Result<()> {
    let text = match clipboard::read_text(MAX_CLIPBOARD_BYTES).await {
        Ok(text) => text,
        Err(error) => {
            app.message = Some(format!("clipboard paste unavailable: {error}"));
            return Ok(());
        }
    };
    if text.is_empty() {
        app.message = Some("system clipboard contains no text".to_owned());
        return Ok(());
    }
    let characters = text.chars().count();
    paste_text_into_selected(
        state,
        app,
        text,
        format!("pasted {characters} characters from system clipboard"),
    )
    .await
}

async fn paste_text_into_selected(
    state: &FederationState,
    app: &mut App,
    text: String,
    feedback: String,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    if text.len() > MAX_CLIPBOARD_BYTES {
        app.message = Some(format!(
            "paste exceeded the {} MiB limit; paste not sent",
            MAX_CLIPBOARD_BYTES / (1024 * 1024)
        ));
        return Ok(());
    }
    if text
        .as_bytes()
        .windows(BRACKETED_PASTE_END.len())
        .any(|window| window == BRACKETED_PASTE_END)
    {
        app.message =
            Some("paste contains a bracketed-paste terminator; paste not sent".to_owned());
        return Ok(());
    }

    let Some(selected) = app.selected_pane.clone() else {
        app.message = Some("no terminal pane is selected".to_owned());
        return Ok(());
    };
    let key = selected.target_session();
    let Some(route) = app.routes.get(&selected) else {
        app.message = Some("selected pane has no control route".to_owned());
        return Ok(());
    };
    if route.access != TerminalAccess::Control {
        app.message = Some("read-only: another Herdr client owns control; cannot paste".to_owned());
        return Ok(());
    }
    let Some(runtime) = state.targets.get(&key) else {
        app.message = Some("selected target is unavailable".to_owned());
        return Ok(());
    };
    if !runtime.accepts_generation(route.generation) {
        app.routes.remove(&selected);
        app.message = Some("control route became stale".to_owned());
        return Ok(());
    }
    // An atomic paste goes through the session's own socket so Herdr writes it
    // in one piece, and only the daemon has a route there. Which targets can do
    // that is the daemon's knowledge too, so it is asked rather than guessed at
    // from configuration this frontend happens to hold.
    if atomic_paste_available(&app.resolved_targets, &key) {
        let Some(client) = app.client.clone() else {
            app.message = Some("paste routing is unavailable; paste not sent".to_owned());
            return Ok(());
        };
        let request = client.paste_pane_text(selected.clone(), text);
        app.pending_operations.insert(
            request,
            PendingOperation {
                description: "pasted terminal text".to_owned(),
                follow_server_focus: false,
                clipboard_feedback: Some(feedback),
            },
        );
        app.message = Some("sending one atomic paste…".to_owned());
        return Ok(());
    }

    let bracketed = route.parser.screen().bracketed_paste();
    if !bracketed && text.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
        app.message = Some(
            "atomic multiline paste requires session discovery or a configured Herdr API socket; paste not sent"
                .to_owned(),
        );
        return Ok(());
    }
    let payload = terminal_paste_payload(text.as_bytes(), bracketed)?;
    let controlling = app
        .routes
        .get(&selected)
        .is_some_and(|route| route.access == TerminalAccess::Control);
    let Some(client) = app.client.clone().filter(|_| controlling) else {
        app.message = Some("terminal control route closed before paste".to_owned());
        return Ok(());
    };
    client.send_input(selected.clone(), payload);
    app.clipboard_feedback = Some(ClipboardFeedback {
        text: feedback,
        expires_at: Instant::now() + CLIPBOARD_FEEDBACK_DURATION,
    });
    Ok(())
}

async fn paste_clipboard_media(state: &FederationState, app: &mut App) -> Result<()> {
    let Some(selected) = app.selected_pane.clone() else {
        app.message = Some("no terminal pane is selected".to_owned());
        return Ok(());
    };
    let key = selected.target_session();
    let Some(runtime) = state.targets.get(&key) else {
        app.message = Some("selected target is unavailable".to_owned());
        return Ok(());
    };
    let Some(route) = app.routes.get(&selected) else {
        app.message = Some("selected pane has no control route".to_owned());
        return Ok(());
    };
    if route.access != TerminalAccess::Control {
        app.message =
            Some("read-only: another Herdr client owns control; cannot paste media".to_owned());
        return Ok(());
    }
    if !runtime.accepts_generation(route.generation) {
        app.routes.remove(&selected);
        app.message = Some("control route became stale".to_owned());
        return Ok(());
    }
    // Reading the clipboard is a desktop-session capability and stays here.
    // Moving the bytes needs a route to the host, so that half is the daemon's.
    let (media, payload) = match clipboard::read_offered_media(MAX_CLIPBOARD_MEDIA_BYTES).await {
        Ok(offered) => offered,
        Err(error) => {
            app.message = Some(format!("clipboard media unavailable: {error}"));
            return Ok(());
        }
    };
    let Some(client) = app.client.clone() else {
        app.message = Some("clipboard upload routing is unavailable".to_owned());
        return Ok(());
    };
    let bytes = payload.len();
    let request = client.upload_media(selected, media.mime.to_owned(), &payload);
    app.pending_uploads.insert(
        request,
        PendingUpload {
            mime: media.mime.to_owned(),
        },
    );
    app.message = Some(format!("uploading {} {bytes} bytes…", media.mime));
    Ok(())
}

fn terminal_paste_payload(text: &[u8], bracketed: bool) -> Result<Vec<u8>> {
    if text
        .windows(BRACKETED_PASTE_END.len())
        .any(|window| window == BRACKETED_PASTE_END)
    {
        bail!("clipboard text contains a bracketed-paste terminator");
    }
    if !bracketed {
        return Ok(text.to_vec());
    }
    let mut payload =
        Vec::with_capacity(BRACKETED_PASTE_START.len() + text.len() + BRACKETED_PASTE_END.len());
    payload.extend_from_slice(BRACKETED_PASTE_START);
    payload.extend_from_slice(text);
    payload.extend_from_slice(BRACKETED_PASTE_END);
    Ok(payload)
}

fn cycle_pane(state: &FederationState, app: &mut App, direction: isize) {
    let panes = selectable_panes(state);
    if panes.is_empty() {
        app.selected_pane = None;
        app.selection = None;
        app.selection_autoscroll = None;
        app.routes.clear();
        app.route_retry_after.clear();
        return;
    }
    app.selection_explicit = true;
    let current = app
        .selected_pane
        .as_ref()
        .and_then(|selected| panes.iter().position(|pane| pane == selected))
        .unwrap_or(0);
    let next = wrapped_index(current, panes.len(), direction);
    if app.selected_pane.as_ref() != Some(&panes[next]) {
        select_pane(app, panes[next].clone());
    }
}

fn cycle_tab(state: &FederationState, app: &mut App, direction: isize) {
    let Some((snapshot, pane)) = selected_snapshot_and_pane(state, app.selected_pane.as_ref())
    else {
        return;
    };
    let Some(workspace) = pane.workspace.as_ref() else {
        return;
    };
    let tabs = snapshot
        .tabs
        .values()
        .filter(|tab| tab.workspace.as_ref() == Some(workspace))
        .map(|tab| tab.id.clone())
        .collect::<Vec<_>>();
    if tabs.is_empty() {
        return;
    }
    let current = pane
        .tab
        .as_ref()
        .and_then(|selected| tabs.iter().position(|tab| tab == selected))
        .unwrap_or(0);
    let next = wrapped_index(current, tabs.len(), direction);
    select_tab(snapshot, &tabs[next], app);
}

fn select_tab_index(state: &FederationState, app: &mut App, index: usize) {
    let Some((snapshot, pane)) = selected_snapshot_and_pane(state, app.selected_pane.as_ref())
    else {
        return;
    };
    let Some(workspace) = pane.workspace.as_ref() else {
        return;
    };
    let tabs = snapshot
        .tabs
        .values()
        .filter(|tab| tab.workspace.as_ref() == Some(workspace))
        .collect::<Vec<_>>();
    let Some(tab) = tabs.get(index) else {
        app.message = Some(format!("Herdr: tab {} is not available", index + 1));
        return;
    };
    select_tab(snapshot, &tab.id, app);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneDirection {
    Left,
    Right,
    Up,
    Down,
}

fn select_neighbor_pane(state: &FederationState, app: &mut App, direction: PaneDirection) {
    let Some((snapshot, pane)) = selected_snapshot_and_pane(state, app.selected_pane.as_ref())
    else {
        return;
    };
    let Some(tab) = pane.tab.as_ref() else {
        return;
    };
    let Some(layout) = snapshot.layouts.get(tab) else {
        app.message = Some("Herdr: selected tab has no pane layout".to_owned());
        return;
    };
    let Some(current) = layout
        .panes
        .iter()
        .find(|layout_pane| layout_pane.pane == pane.id)
    else {
        return;
    };
    let next = layout
        .panes
        .iter()
        .filter(|candidate| candidate.pane != pane.id)
        .filter_map(|candidate| {
            pane_direction_score(current.rect, candidate.rect, direction)
                .map(|score| (score, candidate.pane.clone()))
        })
        .min_by_key(|(score, pane)| (*score, pane.clone()));
    if let Some((_, pane)) = next {
        select_pane(app, pane);
    } else {
        app.message = Some(format!("Herdr: no pane to the {}", direction.label()));
    }
}

impl PaneDirection {
    fn label(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "top",
            Self::Down => "bottom",
        }
    }
}

fn pane_direction_score(
    current: crate::state::LayoutRect,
    candidate: crate::state::LayoutRect,
    direction: PaneDirection,
) -> Option<(u32, u32)> {
    let current_right = u32::from(current.x) + u32::from(current.width);
    let candidate_right = u32::from(candidate.x) + u32::from(candidate.width);
    let current_bottom = u32::from(current.y) + u32::from(current.height);
    let candidate_bottom = u32::from(candidate.y) + u32::from(candidate.height);
    let current_x = u32::from(current.x);
    let candidate_x = u32::from(candidate.x);
    let current_y = u32::from(current.y);
    let candidate_y = u32::from(candidate.y);
    match direction {
        PaneDirection::Left if candidate_right <= current_x => Some((
            current_x - candidate_right,
            interval_distance(current_y, current_bottom, candidate_y, candidate_bottom),
        )),
        PaneDirection::Right if candidate_x >= current_right => Some((
            candidate_x - current_right,
            interval_distance(current_y, current_bottom, candidate_y, candidate_bottom),
        )),
        PaneDirection::Up if candidate_bottom <= current_y => Some((
            current_y - candidate_bottom,
            interval_distance(current_x, current_right, candidate_x, candidate_right),
        )),
        PaneDirection::Down if candidate_y >= current_bottom => Some((
            candidate_y - current_bottom,
            interval_distance(current_x, current_right, candidate_x, candidate_right),
        )),
        _ => None,
    }
}

fn interval_distance(first_start: u32, first_end: u32, second_start: u32, second_end: u32) -> u32 {
    if first_end < second_start {
        second_start - first_end
    } else {
        first_start.saturating_sub(second_end)
    }
}

fn select_workspace(state: &FederationState, app: &mut App, index: usize) {
    let workspaces = state
        .targets
        .values()
        .filter(|target| target.connection == TargetConnectionState::Live)
        .filter_map(|target| target.snapshot.as_deref())
        .flat_map(|snapshot| {
            snapshot
                .workspaces
                .values()
                .map(move |workspace| (snapshot, workspace))
        })
        .collect::<Vec<_>>();
    let Some((snapshot, workspace)) = workspaces.get(index).copied() else {
        app.message = Some(format!("workspace {} is not available", index + 1));
        return;
    };
    let tab = workspace.active_tab.as_ref().or_else(|| {
        snapshot
            .tabs
            .values()
            .find(|tab| tab.workspace.as_ref() == Some(&workspace.id))
            .map(|tab| &tab.id)
    });
    if let Some(tab) = tab {
        select_tab(snapshot, tab, app);
    }
}

fn select_tab(snapshot: &NormalizedSnapshot, tab: &crate::model::TabId, app: &mut App) {
    let pane = snapshot
        .layouts
        .get(tab)
        .map(|layout| layout.focused_pane.clone())
        .or_else(|| {
            snapshot
                .panes
                .values()
                .find(|pane| pane.tab.as_ref() == Some(tab))
                .map(|pane| pane.id.clone())
        });
    if let Some(pane) = pane {
        select_pane(app, pane);
    }
}

fn wrapped_index(current: usize, length: usize, direction: isize) -> usize {
    if direction < 0 {
        current
            .checked_sub(direction.unsigned_abs())
            .unwrap_or(length - 1)
    } else {
        current.saturating_add(direction as usize) % length
    }
}

fn reconcile_selection(state: &FederationState, app: &mut App) {
    let panes = selectable_panes(state);
    if let Some(persisted) = app.restore_pending.clone() {
        match state.targets.get(&persisted.target_session()) {
            Some(target) if target.connection == TargetConnectionState::Live => {
                app.restore_pending = None;
                if panes.contains(&persisted) {
                    app.selected_pane = Some(persisted);
                    app.selection_explicit = true;
                    return;
                }
            }
            Some(_) => {}
            None => app.restore_pending = None,
        }
    }
    let selection_is_valid = app
        .selected_pane
        .as_ref()
        .is_some_and(|selected| panes.contains(selected));
    if selection_is_valid && app.selection_explicit {
        return;
    }

    if !selection_is_valid {
        app.selection_explicit = false;
    }

    let startup_pane = state
        .targets
        .values()
        .filter(|target| target.connection == TargetConnectionState::Live)
        .filter_map(|target| target.snapshot.as_deref())
        .max_by_key(|snapshot| {
            (
                snapshot.counts.agents,
                snapshot.counts.workspaces,
                snapshot.counts.panes,
            )
        })
        .and_then(|snapshot| {
            snapshot
                .focused_pane
                .clone()
                .or_else(|| snapshot.panes.keys().next().cloned())
        });

    if app.selected_pane != startup_pane {
        app.selected_pane = startup_pane;
        app.selection = None;
        app.selection_autoscroll = None;
        app.route_retry_after.clear();
        app.message = None;
    }
}

fn selectable_panes(state: &FederationState) -> Vec<PaneId> {
    state
        .targets
        .values()
        .filter(|target| target.connection == TargetConnectionState::Live)
        .filter_map(|target| target.snapshot.as_deref())
        .flat_map(|snapshot| snapshot.panes.keys().cloned())
        .collect()
}

/// Reconcile the daemon subscriptions with what the layout wants on screen.
///
/// The terminals themselves live with the daemon; what this keeps is one screen
/// model per visible pane. A subscription is replaced rather than adjusted when
/// its size, its requested access, or its target's connection generation
/// changes, which is the same rule the frontend applied when it spawned these
/// routes itself.
fn ensure_routes(
    state: &FederationState,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<()> {
    let Some(client) = app.client.clone() else {
        return Ok(());
    };
    let Some(selected) = app.selected_pane.clone() else {
        for pane in std::mem::take(&mut app.routes).into_keys() {
            client.unsubscribe_pane(pane);
        }
        return Ok(());
    };
    let area = terminal.size().context("failed to read terminal size")?;
    let frame_area = area.into();
    let (_, _, terminal_area) = ui_areas(frame_area);
    app.last_frame_area = Some(frame_area);
    app.last_terminal_area = Some(terminal_area);
    let desired = visible_pane_areas(state, Some(&selected), terminal_area)
        .into_iter()
        .filter_map(|(pane, area)| {
            let inner = pane_block(&pane, pane == selected, None).inner(area);
            (inner.width > 0 && inner.height > 0).then_some((pane, inner))
        })
        .collect::<BTreeMap<_, _>>();

    let mut released = Vec::new();
    app.routes.retain(|pane, route| {
        let keep = desired.get(pane).is_some_and(|inner| {
            state
                .targets
                .get(&pane.target_session())
                .is_some_and(|runtime| {
                    route.requested == desired_access(pane, &selected)
                        && route.generation == runtime.connection_generation
                        && route.rows == inner.height
                        && route.columns == inner.width
                })
        });
        if !keep {
            released.push(pane.clone());
        }
        keep
    });
    for pane in released {
        client.unsubscribe_pane(pane);
    }

    for (pane, inner) in desired {
        if app.routes.contains_key(&pane)
            || app
                .route_retry_after
                .get(&pane)
                .is_some_and(|retry| *retry > Instant::now())
        {
            continue;
        }
        let Some(runtime) = state.targets.get(&pane.target_session()) else {
            continue;
        };
        let requested = desired_access(&pane, &selected);
        client.subscribe_pane(pane.clone(), requested, inner.width, inner.height);
        app.routes.insert(
            pane.clone(),
            ActiveRoute {
                pane,
                requested,
                // Until the daemon answers with a lease, assume the lesser of what
                // was asked for, so nothing types into a pane it may not own.
                access: TerminalAccess::Observe,
                generation: runtime.connection_generation,
                rows: inner.height,
                columns: inner.width,
                parser: vt100::Parser::new(inner.height, inner.width, 2_000),
                last_sequence: None,
            },
        );
    }
    Ok(())
}

/// Only the selected pane asks for control. Whether it gets it is the daemon's
/// answer, not this frontend's assumption.
fn desired_access(pane: &PaneId, selected: &PaneId) -> TerminalAccess {
    if pane == selected {
        TerminalAccess::Control
    } else {
        TerminalAccess::Observe
    }
}

/// Apply one message from the daemon that is not federation state.
fn handle_daemon_event(message: ServerMessage, app: &mut App) {
    match message {
        ServerMessage::PaneFrame {
            pane,
            sequence,
            width,
            height,
            full,
            bytes,
        } => {
            if !app.routes.contains_key(&pane) {
                return;
            }
            let pending_direction = app
                .selection_autoscroll
                .as_ref()
                .filter(|autoscroll| autoscroll.pane == pane && autoscroll.pending_lines > 0)
                .map(|autoscroll| autoscroll.direction);
            let before = pending_direction.and_then(|_| {
                app.routes
                    .get(&pane)
                    .map(|route| capture_screen_rows(route.parser.screen()))
            });
            let after = {
                let route = app.routes.get_mut(&pane).expect("route was checked above");
                if full || route.rows != height || route.columns != width {
                    route.parser = vt100::Parser::new(height, width, 2_000);
                    route.rows = height;
                    route.columns = width;
                }
                route.parser.process(&bytes);
                route.last_sequence = Some(sequence);
                capture_screen_rows(route.parser.screen())
            };
            update_selection_after_frame(app, &pane, pending_direction, before.as_deref(), after);
        }
        ServerMessage::PaneLease { pane, access } => {
            if let Some(route) = app.routes.get_mut(&pane) {
                route.access = access;
            }
            if app.selected_pane.as_ref() == Some(&pane) {
                app.message = match access {
                    TerminalAccess::Control => None,
                    TerminalAccess::Observe => Some(
                        "read-only: another Herdr client owns control; retrying automatically"
                            .to_owned(),
                    ),
                };
            }
        }
        // The TUI owns a `vt100` parser and subscribes for frames, so the daemon
        // never renders on its behalf. Ignored rather than logged: arriving here
        // would mean this client asked for screens, and it has no code that does.
        ServerMessage::PaneScreen { .. } | ServerMessage::PaneScreenDiff { .. } => {}
        ServerMessage::PaneClosed { pane } => {
            app.routes.remove(&pane);
            // Wait before asking again, so a pane that cannot be opened does not
            // become a resubscribe loop.
            app.route_retry_after
                .insert(pane.clone(), Instant::now() + Duration::from_secs(2));
            if app.selected_pane.as_ref() == Some(&pane) {
                app.message = Some("terminal route closed".to_owned());
            }
        }
        ServerMessage::OperationResult {
            request,
            applied,
            message,
        } => {
            app.herdr_action_inflight = false;
            let pending = app.pending_operations.remove(&request);
            if applied {
                match pending
                    .as_ref()
                    .and_then(|pending| pending.clipboard_feedback.clone())
                {
                    Some(feedback) => {
                        app.clipboard_feedback = Some(ClipboardFeedback {
                            text: feedback,
                            expires_at: Instant::now() + CLIPBOARD_FEEDBACK_DURATION,
                        });
                        app.message = None;
                    }
                    None => {
                        app.message = Some(format!(
                            "Herdr: {}",
                            pending
                                .as_ref()
                                .map_or(message.as_str(), |pending| pending.description.as_str())
                        ));
                    }
                }
                if pending.is_some_and(|pending| pending.follow_server_focus) {
                    app.selection_explicit = false;
                    app.selected_pane = None;
                }
            } else {
                app.message = Some(format!("Herdr action failed: {message}"));
            }
        }
        // The frontend uploads what a clipboard held, in one attempt, over a
        // socket it shares a machine with. It has nothing to resume from and
        // nowhere to keep a token, so it notes neither — a client that outlives
        // its connection is the one these are for.
        ServerMessage::UploadAccepted { .. } | ServerMessage::UploadInterrupted { .. } => {}
        // The frontend does not ask for files off a host: it has a shell on
        // one, and somewhere to put what it reads is a question about this
        // machine's filesystem that no key in the TUI currently asks.
        ServerMessage::DownloadOffer { .. }
        | ServerMessage::DownloadChunk { .. }
        | ServerMessage::DownloadFinished { .. } => {}
        ServerMessage::UploadComplete {
            request,
            path,
            bytes,
        } => {
            let mime = app
                .pending_uploads
                .remove(&request)
                .map_or_else(|| "payload".to_owned(), |pending| pending.mime);
            // Only a verified path is ever injected, and pasting it happens
            // here because bracketed-paste state lives with the parser.
            paste_verified_path(
                app,
                &path,
                format!("uploaded and verified {mime} {bytes} bytes; pasted remote path"),
            );
        }
        ServerMessage::PairingCode {
            code,
            expires_in_seconds,
            ..
        } => {
            let minutes = expires_in_seconds / 60;
            app.message = Some(format!(
                "pairing code {code} — enter it in the browser client within {minutes} minutes"
            ));
        }
        ServerMessage::Error { request, message } => {
            // A refusal ends whatever asked for it, so a failed transfer does
            // not leave the frontend waiting on a result that will not come.
            if let Some(request) = request {
                app.pending_uploads.remove(&request);
                if app.pending_operations.remove(&request).is_some() {
                    app.herdr_action_inflight = false;
                }
            }
            app.message = Some(message);
        }
        ServerMessage::Attention { event } => {
            if event.unread && app.attention.apply(event) {
                app.attention_sidebar_offset = 0;
            }
        }
        ServerMessage::AttentionHistory { events } => {
            app.attention = AttentionIndex::mirror(events);
        }
        ServerMessage::Hello { .. }
        | ServerMessage::FederationState { .. }
        | ServerMessage::TargetState { .. } => {}
    }
}

fn update_selection_after_frame(
    app: &mut App,
    pane: &PaneId,
    pending_direction: Option<SelectionAutoscrollDirection>,
    before: Option<&[CapturedRow]>,
    after: Vec<CapturedRow>,
) {
    let shift = pending_direction.and_then(|direction| {
        before.and_then(|before| viewport_shift_distance(before, &after, direction))
    });

    let Some(selection) = app
        .selection
        .as_mut()
        .filter(|selection| &selection.pane == pane)
    else {
        app.selection_autoscroll = None;
        return;
    };
    if let Some(lines) = shift {
        let direction = pending_direction.expect("shifted viewport has a direction");
        let lines = i64::try_from(lines).unwrap_or(i64::MAX);
        selection.viewport_offset += match direction {
            SelectionAutoscrollDirection::Up => lines,
            SelectionAutoscrollDirection::Down => -lines,
        };
        let viewport_row = match direction {
            SelectionAutoscrollDirection::Up => 0,
            SelectionAutoscrollDirection::Down => {
                u16::try_from(after.len().saturating_sub(1)).unwrap_or(u16::MAX)
            }
        };
        let column = app
            .selection_autoscroll
            .as_ref()
            .map_or(selection.head.column, |autoscroll| autoscroll.column);
        selection.head =
            CellPosition::from_viewport(viewport_row, column, selection.viewport_offset);
        capture_selection_rows(selection, after);
        if let Some(autoscroll) = app.selection_autoscroll.as_mut() {
            autoscroll.pending_lines = autoscroll
                .pending_lines
                .saturating_sub(usize::try_from(lines).unwrap_or(usize::MAX));
        }
    } else {
        capture_selection_rows(selection, after);
    }
}

fn viewport_shift_distance(
    before: &[CapturedRow],
    after: &[CapturedRow],
    direction: SelectionAutoscrollDirection,
) -> Option<usize> {
    if before.len() != after.len() || before.is_empty() || before == after {
        return None;
    }
    if before.len() == 1 {
        return Some(1);
    }
    (1..before.len()).find(|lines| match direction {
        SelectionAutoscrollDirection::Up => before[..before.len() - lines] == after[*lines..],
        SelectionAutoscrollDirection::Down => before[*lines..] == after[..after.len() - lines],
    })
}

fn render(frame: &mut Frame, state: &FederationState, app: &App) {
    let (sidebar_area, tab_area, terminal_area) = ui_areas(frame.area());
    if sidebar_area.width > 0 {
        render_sidebar(frame, state, app, sidebar_area);
    }
    render_tabs(frame, state, app, tab_area);
    render_terminal_surfaces(frame, state, app, terminal_area);
    if let Some(manager) = app.target_manager.as_ref() {
        render_target_manager(frame, manager);
    } else if let Some(navigator) = app.agent_navigator.as_ref() {
        render_agent_navigator(frame, state, navigator);
    } else if let Some(center) = app.attention_center.as_ref() {
        render_attention_center(frame, &app.attention, center);
    } else if let Some(menu) = app.context_menu.as_ref() {
        render_context_menu(frame, menu);
    } else if let Some(palette) = app.command_palette.as_ref() {
        render_command_palette(frame, state, app.selected_pane.as_ref(), palette);
    } else if let Some(prompt) = app.text_prompt.as_ref() {
        render_text_prompt(frame, prompt);
    } else if let Some(confirmation) = app.close_confirmation.as_ref() {
        render_close_confirmation(frame, confirmation);
    }
}

fn context_menu_block(title: &str) -> Block<'static> {
    Block::default()
        .title(Span::styled(
            format!(" {} ", safe_text(title)),
            Style::default().bold(),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .padding(Padding::horizontal(1))
}

fn context_menu_area(frame_area: Rect, menu: &ContextMenu) -> Rect {
    let label_width = menu
        .actions
        .iter()
        .map(|action| action.palette_label().chars().count())
        .max()
        .unwrap_or_default();
    let desired_width = menu
        .title
        .chars()
        .count()
        .max(label_width)
        .saturating_add(6);
    let width = u16::try_from(desired_width)
        .unwrap_or(u16::MAX)
        .min(frame_area.width)
        .max(1);
    let desired_height = menu.actions.len().saturating_add(2);
    let height = u16::try_from(desired_height)
        .unwrap_or(u16::MAX)
        .min(frame_area.height)
        .max(1);
    let maximum_x = frame_area
        .x
        .saturating_add(frame_area.width.saturating_sub(width));
    let maximum_y = frame_area
        .y
        .saturating_add(frame_area.height.saturating_sub(height));
    Rect::new(
        menu.anchor.x.clamp(frame_area.x, maximum_x),
        menu.anchor.y.clamp(frame_area.y, maximum_y),
        width,
        height,
    )
}

fn context_menu_action_at(
    frame_area: Rect,
    menu: &ContextMenu,
    position: Position,
) -> Option<usize> {
    let area = context_menu_area(frame_area, menu);
    let inner = context_menu_block(&menu.title).inner(area);
    if !inner.contains(position) {
        return None;
    }
    let index = usize::from(position.y.saturating_sub(inner.y));
    (index < menu.actions.len()).then_some(index)
}

fn render_context_menu(frame: &mut Frame, menu: &ContextMenu) {
    let area = context_menu_area(frame.area(), menu);
    frame.render_widget(Clear, area);
    let block = context_menu_block(&menu.title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines = menu
        .actions
        .iter()
        .enumerate()
        .map(|(index, action)| {
            let style = if action.is_destructive() {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };
            let line = Line::from(vec![
                Span::raw(if index == menu.selected { "> " } else { "  " }),
                Span::styled(safe_text(&action.palette_label()), style),
            ]);
            if index == menu.selected {
                line.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                line
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_command_palette(
    frame: &mut Frame,
    state: &FederationState,
    selected: Option<&PaneId>,
    palette: &CommandPalette,
) {
    let area = centered_popup(frame.area(), 82, 22);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(Span::styled(" actions ", Style::default().bold()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let actions = filtered_palette_actions(state, selected, &palette.query);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::raw(safe_text(&palette.query)),
            Span::styled("_", Style::default().fg(Color::Cyan)),
        ]),
        Line::styled(
            "Type to search   Tab/Ctrl+N next   Ctrl+P previous   Enter run   Esc close",
            Style::default().fg(Color::DarkGray),
        ),
        Line::default(),
    ];
    let available = usize::from(inner.height).saturating_sub(lines.len());
    if actions.is_empty() {
        lines.push(Line::styled(
            "No matching actions",
            Style::default().fg(Color::DarkGray),
        ));
    } else if available > 0 {
        let selected = palette.selected.min(actions.len() - 1);
        let start = selected
            .saturating_add(1)
            .saturating_sub(available)
            .min(actions.len().saturating_sub(available));
        for (index, action) in actions.iter().enumerate().skip(start).take(available) {
            let line = Line::from(vec![
                Span::raw(if index == selected { "> " } else { "  " }),
                Span::raw(safe_text(&action.palette_label())),
                Span::styled(
                    format!("  {}", safe_text(&action.palette_scope())),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            lines.push(if index == selected {
                line.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                line
            });
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_text_prompt(frame: &mut Frame, prompt: &TextPrompt) {
    let area = centered_popup(frame.area(), 72, 10);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(Span::styled(
            format!(" {} ", safe_text(&prompt.title)),
            Style::default().bold(),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = vec![
        Line::from(safe_text(&prompt.label)),
        Line::default(),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::raw(safe_text(&prompt.value)),
            Span::styled("_", Style::default().fg(Color::Cyan)),
        ]),
        Line::default(),
        Line::styled(
            "Enter apply   Ctrl+U clear   Esc cancel",
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if let Some(error) = prompt.error.as_deref() {
        lines.push(Line::styled(
            safe_text(error),
            Style::default().fg(Color::Red),
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn close_confirmation_details(
    action: &ResourceAction,
) -> Option<(&'static str, &str, TargetSession, &str, &'static str)> {
    match action {
        ResourceAction::CloseWorkspace { workspace, label } => Some((
            "workspace",
            label,
            workspace.target_session(),
            &workspace.resource,
            "This closes its tabs and panes; the Herdr session remains running.",
        )),
        ResourceAction::CloseTab { tab, label } => Some((
            "tab",
            label,
            tab.target_session(),
            &tab.resource,
            "This closes every pane in the tab; the Herdr session remains running.",
        )),
        ResourceAction::ClosePane { pane, label } => Some((
            "pane",
            label,
            pane.target_session(),
            &pane.resource,
            "This closes the pane and its process; the Herdr session remains running.",
        )),
        _ => None,
    }
}

fn render_close_confirmation(frame: &mut Frame, confirmation: &CloseConfirmation) {
    let Some((kind, label, target_session, resource, warning)) =
        close_confirmation_details(&confirmation.action)
    else {
        return;
    };
    let area = centered_popup(frame.area(), 72, 11);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(Span::styled(
            format!(" close Herdr {kind} "),
            Style::default().bold(),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!("Close {kind} {label:?}?")),
            Line::default(),
            Line::from(format!(
                "Host/session: {}",
                safe_text(&target_session.to_string())
            )),
            Line::from(format!("Herdr {kind} ID: {}", safe_text(resource))),
            Line::default(),
            Line::styled("y close   n/Esc cancel", Style::default().fg(Color::Yellow)),
            Line::from(warning),
        ]),
        inner,
    );
}

fn target_manager_block() -> Block<'static> {
    Block::default()
        .title(Span::styled(" target manager ", Style::default().bold()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .padding(Padding::horizontal(1))
}

fn target_manager_area(frame_area: Rect) -> Rect {
    centered_popup(frame_area, 78, 28)
}

fn target_manager_inner(frame_area: Rect) -> Rect {
    target_manager_block().inner(target_manager_area(frame_area))
}

fn target_manager_list_offset(manager: &TargetManager, visible_rows: usize) -> usize {
    if visible_rows == 0 || manager.selected < visible_rows {
        0
    } else {
        manager
            .selected
            .saturating_add(1)
            .saturating_sub(visible_rows)
    }
}

fn discovered_session_offset(form: &TargetForm, visible_rows: usize) -> usize {
    let maximum = form.discovered_sessions.len().saturating_sub(visible_rows);
    let mut offset = form.discovered_offset.min(maximum);
    if visible_rows == 0 {
        return offset;
    }
    if form.discovered_selected < offset {
        offset = form.discovered_selected;
    } else if form.discovered_selected >= offset.saturating_add(visible_rows) {
        offset = form
            .discovered_selected
            .saturating_add(1)
            .saturating_sub(visible_rows);
    }
    offset.min(maximum)
}

fn target_manager_controls(
    frame_area: Rect,
    manager: &TargetManager,
) -> Vec<(Rect, TargetManagerControl)> {
    let inner = target_manager_inner(frame_area);
    let mut controls = Vec::new();
    let mut add_control = |x: u16, y: u16, width: u16, control| {
        if y < inner.height && x < inner.width {
            controls.push((
                Rect::new(
                    inner.x.saturating_add(x),
                    inner.y.saturating_add(y),
                    width.min(inner.width.saturating_sub(x)),
                    1,
                ),
                control,
            ));
        }
    };
    match &manager.mode {
        TargetManagerMode::List => {
            add_control(0, 2, 7, TargetManagerControl::Add);
            add_control(9, 2, 8, TargetManagerControl::Edit);
            add_control(19, 2, 10, TargetManagerControl::Remove);
            add_control(31, 2, 9, TargetManagerControl::Close);
            let visible_rows = usize::from(inner.height.saturating_sub(5));
            let offset = target_manager_list_offset(manager, visible_rows);
            for (visible, (index, _)) in manager
                .targets
                .iter()
                .enumerate()
                .skip(offset)
                .take(visible_rows)
                .enumerate()
            {
                let Ok(visible) = u16::try_from(visible) else {
                    break;
                };
                add_control(
                    0,
                    5 + visible,
                    inner.width,
                    TargetManagerControl::Target(index),
                );
            }
        }
        TargetManagerMode::Form(form) => {
            add_control(
                0,
                2,
                inner.width,
                TargetManagerControl::Field(TargetFormField::Name),
            );
            add_control(
                0,
                3,
                inner.width,
                TargetManagerControl::Field(TargetFormField::Ssh),
            );
            add_control(
                0,
                4,
                inner.width,
                TargetManagerControl::Field(TargetFormField::Session),
            );
            add_control(
                0,
                5,
                inner.width,
                TargetManagerControl::Field(TargetFormField::DiscoverSessions),
            );
            add_control(0, 7, 19, TargetManagerControl::TestConnection);
            add_control(21, 7, 8, TargetManagerControl::Save);
            add_control(31, 7, 10, TargetManagerControl::Cancel);
            let visible_rows = usize::from(inner.height.saturating_sub(13));
            let offset = discovered_session_offset(form, visible_rows);
            for (visible, (index, _)) in form
                .discovered_sessions
                .iter()
                .enumerate()
                .skip(offset)
                .take(visible_rows)
                .enumerate()
            {
                let Ok(visible) = u16::try_from(visible) else {
                    break;
                };
                add_control(
                    0,
                    13 + visible,
                    inner.width,
                    TargetManagerControl::DiscoveredSession(index),
                );
            }
        }
        TargetManagerMode::ConfirmRemove { .. } => {
            add_control(0, 3, 10, TargetManagerControl::ConfirmRemove);
            add_control(12, 3, 10, TargetManagerControl::CancelRemove);
        }
    }
    controls
}

fn render_target_manager(frame: &mut Frame, manager: &TargetManager) {
    let area = target_manager_area(frame.area());
    frame.render_widget(Clear, area);
    let block = target_manager_block();
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = match &manager.mode {
        TargetManagerMode::List => {
            let mut lines = vec![
                Line::styled(
                    "Click a machine or use j/k; actions are local to Super-Herdr",
                    Style::default().fg(Color::DarkGray),
                ),
                Line::default(),
                Line::from("[ Add ]  [ Edit ]  [ Remove ]  [ Close ]"),
                Line::default(),
                Line::styled(
                    format!("  {:<18} {:<22} {}", "NAME", "ENDPOINT", "SESSION SCOPE"),
                    Style::default().fg(Color::DarkGray),
                ),
            ];
            let visible_rows = usize::from(inner.height.saturating_sub(5));
            let offset = target_manager_list_offset(manager, visible_rows);
            for (index, target) in manager.targets.iter().enumerate().skip(offset) {
                let selected = index == manager.selected;
                let scope = if target.discover_sessions {
                    "running sessions"
                } else {
                    target.session_name()
                };
                let line = Line::from(format!(
                    "{} {:<18} {:<22} {}",
                    if selected { ">" } else { " " },
                    safe_text(&target.name),
                    safe_text(target.endpoint()),
                    safe_text(scope)
                ));
                lines.push(if selected {
                    line.style(Style::default().add_modifier(Modifier::REVERSED))
                } else {
                    line
                });
            }
            if manager.targets.is_empty() {
                lines.push(Line::from("No targets configured. Press a to add one."));
            }
            lines
        }
        TargetManagerMode::Form(form) => {
            let field_line = |field, label: &str, value: String| {
                let line = Line::from(format!("{label:<27} {value}"));
                if form.field == field {
                    line.style(Style::default().add_modifier(Modifier::REVERSED))
                } else {
                    line
                }
            };
            let mut lines = vec![
                Line::styled(
                    if form.original_name.is_some() {
                        "Edit machine · click fields/buttons · Ctrl+T test · Enter save"
                    } else {
                        "Add machine · click fields/buttons · Ctrl+T test · Enter save"
                    },
                    Style::default().fg(Color::DarkGray),
                ),
                Line::default(),
                field_line(TargetFormField::Name, "Super-Herdr name", form.name.clone()),
                field_line(
                    TargetFormField::Ssh,
                    "SSH alias (blank = local)",
                    form.ssh.clone(),
                ),
                field_line(
                    TargetFormField::Session,
                    "Session/fallback (optional)",
                    form.session.clone(),
                ),
                field_line(
                    TargetFormField::DiscoverSessions,
                    "Discover running sessions",
                    if form.discover_sessions { "[x]" } else { "[ ]" }.to_owned(),
                ),
                Line::default(),
                Line::from("[ Test & discover ]  [ Save ]  [ Cancel ]"),
                Line::default(),
            ];
            let validation_error = form.validation_error(&manager.targets);
            let (status, style) = if let Some(error) = form.error.as_deref() {
                (safe_text(error), Style::default().fg(Color::Red))
            } else if let Some(error) = validation_error.as_deref() {
                (
                    format!("Fix before saving: {}", safe_text(error)),
                    Style::default().fg(Color::Red),
                )
            } else {
                match &form.test_state {
                    TargetTestState::Untested => (
                        "Configuration is valid; connection not tested".to_owned(),
                        Style::default().fg(Color::Yellow),
                    ),
                    TargetTestState::Testing { .. } => (
                        "Testing through the documented Herdr session registry…".to_owned(),
                        Style::default().fg(Color::Yellow),
                    ),
                    TargetTestState::Passed { summary } => {
                        (safe_text(summary), Style::default().fg(Color::Green))
                    }
                    TargetTestState::Failed { message } => (
                        format!("Connection failed: {}", safe_text(message)),
                        Style::default().fg(Color::Red),
                    ),
                }
            };
            lines.push(Line::styled(status, style));
            lines.push(Line::styled(
                "SSH authentication stays in OpenSSH; this test never changes Herdr.",
                Style::default().fg(Color::DarkGray),
            ));
            lines.push(Line::default());
            if !form.discovered_sessions.is_empty() {
                lines.push(Line::styled(
                    "Running sessions · click one or focus with Tab and press Enter",
                    Style::default().fg(Color::Cyan),
                ));
                let visible_rows = usize::from(inner.height.saturating_sub(13));
                let offset = discovered_session_offset(form, visible_rows);
                for (index, session) in form
                    .discovered_sessions
                    .iter()
                    .enumerate()
                    .skip(offset)
                    .take(visible_rows)
                {
                    let selected = index == form.discovered_selected;
                    let line = Line::from(format!(
                        "{} {:<24} use only this session",
                        if selected { ">" } else { " " },
                        safe_text(&session.name)
                    ));
                    lines.push(
                        if selected && form.field == TargetFormField::DiscoveredSession {
                            line.style(Style::default().add_modifier(Modifier::REVERSED))
                        } else if selected {
                            line.style(Style::default().fg(Color::Cyan))
                        } else {
                            line
                        },
                    );
                }
            }
            lines
        }
        TargetManagerMode::ConfirmRemove { name } => vec![
            Line::from(format!(
                "Remove target {:?} from Super-Herdr configuration?",
                safe_text(name)
            )),
            Line::default(),
            Line::default(),
            Line::from("[ Remove ]  [ Cancel ]"),
            Line::from("This does not stop, restart, or alter any Herdr session."),
        ],
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

fn centered_popup(area: Rect, maximum_width: u16, maximum_height: u16) -> Rect {
    let width = maximum_width.min(area.width.saturating_sub(2)).max(1);
    let height = maximum_height.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn render_agent_navigator(frame: &mut Frame, state: &FederationState, navigator: &AgentNavigator) {
    let area = centered_popup(frame.area(), 76, 22);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(Span::styled(" agent navigator ", Style::default().bold()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let entries = agent_jump_entries(state, navigator.filter);
    let mut lines = vec![
        Line::styled(
            format!(
                "j/k select   Enter jump   f filter [{}]   q close",
                navigator.filter.label()
            ),
            Style::default().fg(Color::DarkGray),
        ),
        Line::default(),
    ];
    for (index, entry) in entries.iter().enumerate() {
        let ready = if entry.interactive_ready {
            " input"
        } else {
            ""
        };
        let line = Line::from(format!(
            "{} {:<14} {:<12} {:<18} {}{}",
            if index == navigator.selected {
                ">"
            } else {
                " "
            },
            entry.agent,
            entry.status,
            format!("{}/{}", entry.pane.target, entry.pane.session),
            entry.workspace,
            ready,
        ));
        lines.push(if index == navigator.selected {
            line.style(Style::default().add_modifier(Modifier::REVERSED))
        } else {
            line
        });
    }
    if entries.is_empty() {
        lines.push(Line::from(format!(
            "No agents match the {} filter.",
            navigator.filter.label()
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_attention_center(
    frame: &mut Frame,
    attention: &AttentionIndex,
    center: &AttentionCenter,
) {
    let area = centered_popup(frame.area(), 88, 24);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(Span::styled(" attention history ", Style::default().bold()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let events = attention.events().rev().collect::<Vec<_>>();
    let mut lines = vec![
        Line::styled(
            format!(
                "j/k select   Enter jump/read   r mark all read   c clear read   q close   {} unread",
                attention.unread_count()
            ),
            Style::default().fg(Color::DarkGray),
        ),
        Line::default(),
    ];
    let available = usize::from(inner.height).saturating_sub(lines.len());
    if events.is_empty() {
        lines.push(Line::styled(
            "No agent transitions have been recorded.",
            Style::default().fg(Color::DarkGray),
        ));
    } else if available > 0 {
        let selected = center.selected.min(events.len() - 1);
        let start = selected
            .saturating_add(1)
            .saturating_sub(available)
            .min(events.len().saturating_sub(available));
        for (index, event) in events.iter().enumerate().skip(start).take(available) {
            let marker = if event.unread { "●" } else { " " };
            let event_color = attention_event_color(event.kind);
            let line = Line::from(vec![
                Span::styled(
                    format!("{marker} {:>4}  ", attention_event_age(event)),
                    Style::default().fg(if event.unread {
                        Color::White
                    } else {
                        Color::DarkGray
                    }),
                ),
                Span::styled(
                    format!("{:<14}", event.kind.label()),
                    Style::default().fg(event_color),
                ),
                Span::styled(
                    format!(" {:<16}", safe_text(&event.agent)),
                    Style::default().add_modifier(if event.unread {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                ),
                Span::styled(
                    format!(
                        " {}/{}  {}",
                        safe_text(&event.pane.target),
                        short_session(&event.pane.session),
                        safe_text(&event.workspace)
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            lines.push(if index == selected {
                line.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                line
            });
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn attention_event_age(event: &AttentionEvent) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let seconds = now_ms.saturating_sub(event.occurred_at_ms) / 1000;
    if seconds < 60 {
        "now".to_owned()
    } else if seconds < 60 * 60 {
        format!("{}m", seconds / 60)
    } else if seconds < 24 * 60 * 60 {
        format!("{}h", seconds / (60 * 60))
    } else {
        format!("{}d", seconds / (24 * 60 * 60))
    }
}

fn ui_areas(area: Rect) -> (Rect, Rect, Rect) {
    let sidebar_width = if area.width >= 70 {
        SIDEBAR_WIDTH.min(area.width.saturating_sub(20))
    } else {
        0
    };
    let [sidebar, main] =
        Layout::horizontal([Constraint::Length(sidebar_width), Constraint::Min(1)]).areas(area);
    let [tabs, terminal] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(main);
    (sidebar, tabs, terminal)
}

fn pane_block(pane: &PaneId, selected: bool, access: Option<TerminalAccess>) -> Block<'static> {
    let border_color = if selected {
        Color::Blue
    } else {
        Color::DarkGray
    };
    Block::default()
        .title(Span::styled(
            format!(
                " {}{} ",
                safe_text(&pane.resource),
                match access {
                    Some(TerminalAccess::Control) => " [control]",
                    Some(TerminalAccess::Observe) => " [read-only]",
                    None => "",
                }
            ),
            Style::default().fg(Color::Gray),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .padding(Padding::horizontal(1))
}

fn visible_pane_areas(
    state: &FederationState,
    selected: Option<&PaneId>,
    destination: Rect,
) -> Vec<(PaneId, Rect)> {
    let Some((snapshot, selected_state)) = selected_snapshot_and_pane(state, selected) else {
        return Vec::new();
    };
    let Some(selected) = selected else {
        return Vec::new();
    };
    let Some(tab) = selected_state.tab.as_ref() else {
        return vec![(selected.clone(), destination)];
    };
    let Some(layout) = snapshot.layouts.get(tab) else {
        return vec![(selected.clone(), destination)];
    };
    if layout.zoomed {
        return vec![(selected.clone(), destination)];
    }

    let panes = layout
        .panes
        .iter()
        .filter(|pane| snapshot.panes.contains_key(&pane.pane))
        .filter_map(|pane| {
            scale_layout_rect(layout.area, pane.rect, destination)
                .map(|area| (pane.pane.clone(), area))
        })
        .collect::<Vec<_>>();
    if panes.is_empty() {
        vec![(selected.clone(), destination)]
    } else {
        panes
    }
}

fn scale_layout_rect(
    source: crate::state::LayoutRect,
    pane: crate::state::LayoutRect,
    destination: Rect,
) -> Option<Rect> {
    if source.width == 0 || source.height == 0 || destination.width == 0 || destination.height == 0
    {
        return None;
    }
    let left = scale_edge(
        pane.x.saturating_sub(source.x),
        source.width,
        destination.width,
    );
    let right = scale_edge(
        pane.x.saturating_sub(source.x).saturating_add(pane.width),
        source.width,
        destination.width,
    );
    let top = scale_edge(
        pane.y.saturating_sub(source.y),
        source.height,
        destination.height,
    );
    let bottom = scale_edge(
        pane.y.saturating_sub(source.y).saturating_add(pane.height),
        source.height,
        destination.height,
    );
    let left = left.min(destination.width);
    let right = right.min(destination.width);
    let top = top.min(destination.height);
    let bottom = bottom.min(destination.height);
    (right > left && bottom > top).then_some(Rect::new(
        destination.x.saturating_add(left),
        destination.y.saturating_add(top),
        right - left,
        bottom - top,
    ))
}

fn scale_edge(offset: u16, source_length: u16, destination_length: u16) -> u16 {
    ((u32::from(offset) * u32::from(destination_length)) / u32::from(source_length)) as u16
}

#[cfg(test)]
fn sidebar_pane_at_row(
    state: &FederationState,
    selected: Option<&PaneId>,
    row: u16,
) -> Option<PaneId> {
    sidebar_rows(state, selected)
        .get(usize::from(row))
        .and_then(|row| row.pane.clone())
}

#[cfg(test)]
fn sidebar_rows(state: &FederationState, selected: Option<&PaneId>) -> Vec<SidebarRow> {
    sidebar_session_rows(state, selected)
}

fn sidebar_session_rows(state: &FederationState, selected: Option<&PaneId>) -> Vec<SidebarRow> {
    let selected_target = selected.map(PaneId::target_session);
    let selected_workspace = selected_workspace(state, selected);
    let mut rows = Vec::new();
    let mut workspace_index = 1_usize;
    for target in state.targets.values() {
        let is_selected_target = selected_target.as_ref() == Some(&target.key);
        let (symbol, color) = target_status(target);
        let target_style = Style::default()
            .fg(color)
            .add_modifier(if is_selected_target {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        let mut line = Line::from(vec![
            Span::styled(if is_selected_target { ">" } else { " " }, target_style),
            Span::styled(format!("{symbol} "), target_style),
            Span::styled(safe_text(&target.key.target), target_style),
            Span::styled(
                format!("  {}", short_session(&target.key.session)),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                match target.update_mode {
                    TargetUpdateMode::Events => " evt",
                    TargetUpdateMode::Polling if target.event_error.is_some() => " poll!",
                    TargetUpdateMode::Polling => " poll",
                },
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        if is_selected_target {
            line = line.style(Style::default().add_modifier(Modifier::REVERSED));
        }
        let target_pane = (target.connection == TargetConnectionState::Live)
            .then_some(target.snapshot.as_deref())
            .flatten()
            .and_then(|snapshot| {
                snapshot
                    .focused_pane
                    .clone()
                    .or_else(|| snapshot.panes.keys().next().cloned())
            });
        rows.push(SidebarRow {
            line,
            pane: target_pane,
            context: Some(ContextTarget::Session(target.key.clone())),
            selection_anchor: is_selected_target && selected_workspace.is_none(),
        });
        if is_selected_target && let Some(error) = target.event_error.as_deref() {
            rows.push(SidebarRow {
                line: Line::styled(
                    format!("  event: {}", safe_text(error)),
                    Style::default().fg(Color::Red),
                ),
                pane: None,
                context: None,
                selection_anchor: false,
            });
        }
        if let Some(snapshot) = target.snapshot.as_deref() {
            for workspace in snapshot.workspaces.values() {
                let is_selected = selected_workspace.as_ref() == Some(&workspace.id);
                let workspace_style = Style::default()
                    .fg(if is_selected {
                        Color::White
                    } else {
                        Color::Gray
                    })
                    .add_modifier(if is_selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    });
                let mut line = Line::from(vec![
                    Span::styled(
                        if is_selected { ">" } else { " " },
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if workspace_index <= 9 {
                            format!("{} ", workspace_index)
                        } else {
                            "· ".to_owned()
                        },
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        status_symbol(workspace.agent_status.as_deref()),
                        status_style(workspace.agent_status.as_deref()),
                    ),
                    Span::styled(
                        format!(
                            " {}",
                            display_label(&workspace.id.resource, workspace.label.as_deref())
                        ),
                        workspace_style,
                    ),
                ]);
                if is_selected {
                    line = line.style(Style::default().add_modifier(Modifier::REVERSED));
                }
                rows.push(SidebarRow {
                    line,
                    pane: (target.connection == TargetConnectionState::Live)
                        .then(|| pane_for_workspace(snapshot, &workspace.id))
                        .flatten(),
                    context: Some(ContextTarget::Workspace(workspace.id.clone())),
                    selection_anchor: is_selected,
                });
                workspace_index = workspace_index.saturating_add(1);
            }
        }
    }
    rows
}

fn sidebar_attention_rows(
    state: &FederationState,
    selected: Option<&PaneId>,
    attention: &AttentionIndex,
) -> Vec<SidebarRow> {
    let mut rows = Vec::new();
    let waiting = agent_jump_entries(state, AgentFilter::Attention);
    if !waiting.is_empty() {
        rows.push(SidebarRow {
            line: Line::styled(
                format!(" waiting now ({})", waiting.len()),
                Style::default().fg(Color::Red).bold(),
            ),
            pane: None,
            context: None,
            selection_anchor: false,
        });
        for entry in waiting {
            let is_selected = selected == Some(&entry.pane);
            let unread = attention.has_unread_for_pane(&entry.pane);
            let line = Line::from(vec![
                Span::styled(
                    if unread { " ● " } else { " ! " },
                    Style::default().fg(Color::Red),
                ),
                Span::styled(
                    safe_text(&entry.agent),
                    Style::default().fg(Color::Red).bold(),
                ),
                Span::styled(
                    format!(
                        "  {}/{}",
                        safe_text(&entry.pane.target),
                        short_session(&entry.pane.session)
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            rows.push(SidebarRow {
                line: if is_selected {
                    line.style(Style::default().add_modifier(Modifier::REVERSED))
                } else {
                    line
                },
                pane: Some(entry.pane.clone()),
                context: Some(ContextTarget::Pane(entry.pane)),
                selection_anchor: is_selected,
            });
        }
    }

    rows.push(SidebarRow {
        line: Line::styled(
            format!(" history · {} unread  ^]e", attention.unread_count()),
            Style::default().fg(Color::DarkGray),
        ),
        pane: None,
        context: None,
        selection_anchor: false,
    });
    for event in attention.events().rev() {
        let is_selected = selected == Some(&event.pane);
        let line = Line::from(vec![
            Span::styled(
                if event.unread { " ● " } else { "   " },
                Style::default().fg(if event.unread {
                    Color::White
                } else {
                    Color::DarkGray
                }),
            ),
            Span::styled(
                format!("{:>4} ", attention_event_age(event)),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("{} ", attention_event_symbol(event.kind)),
                Style::default().fg(attention_event_color(event.kind)),
            ),
            Span::styled(
                safe_text(&event.agent),
                Style::default().add_modifier(if event.unread {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            Span::styled(
                format!(
                    "  {}/{}",
                    safe_text(&event.pane.target),
                    short_session(&event.pane.session)
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        rows.push(SidebarRow {
            line: if is_selected {
                line.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                line
            },
            pane: Some(event.pane.clone()),
            context: Some(ContextTarget::Pane(event.pane.clone())),
            selection_anchor: is_selected,
        });
    }
    if attention.events().next().is_none() {
        rows.push(SidebarRow {
            line: Line::styled(
                " No recent transitions",
                Style::default().fg(Color::DarkGray),
            ),
            pane: None,
            context: None,
            selection_anchor: false,
        });
    }
    rows
}

fn attention_event_symbol(kind: AttentionEventKind) -> &'static str {
    match kind {
        AttentionEventKind::NeedsAttention => "!",
        AttentionEventKind::Working => "▶",
        AttentionEventKind::Completed => "✓",
        AttentionEventKind::StatusChanged => "•",
        AttentionEventKind::Disappeared => "×",
    }
}

fn attention_event_color(kind: AttentionEventKind) -> Color {
    match kind {
        AttentionEventKind::NeedsAttention => Color::Red,
        AttentionEventKind::Working => Color::Yellow,
        AttentionEventKind::Completed => Color::Green,
        AttentionEventKind::StatusChanged => Color::Cyan,
        AttentionEventKind::Disappeared => Color::DarkGray,
    }
}

fn pane_for_workspace(snapshot: &NormalizedSnapshot, workspace: &WorkspaceId) -> Option<PaneId> {
    let tab = snapshot
        .workspaces
        .get(workspace)
        .and_then(|workspace| workspace.active_tab.as_ref())
        .or_else(|| {
            snapshot
                .tabs
                .values()
                .find(|tab| tab.workspace.as_ref() == Some(workspace))
                .map(|tab| &tab.id)
        })?;
    pane_for_tab(snapshot, tab)
}

fn pane_for_tab(snapshot: &NormalizedSnapshot, tab: &TabId) -> Option<PaneId> {
    snapshot
        .layouts
        .get(tab)
        .map(|layout| layout.focused_pane.clone())
        .or_else(|| {
            snapshot
                .panes
                .values()
                .find(|pane| pane.tab.as_ref() == Some(tab))
                .map(|pane| pane.id.clone())
        })
}

fn tab_at_column(
    state: &FederationState,
    selected: Option<&PaneId>,
    column: u16,
) -> Option<crate::model::TabId> {
    let (snapshot, pane) = selected_snapshot_and_pane(state, selected)?;
    let workspace = pane.workspace.as_ref()?;
    let tabs = snapshot
        .tabs
        .values()
        .filter(|tab| tab.workspace.as_ref() == Some(workspace))
        .collect::<Vec<_>>();
    let mut left = 0_usize;
    let column = usize::from(column);
    for (index, tab) in tabs.iter().enumerate() {
        let title = Line::from(format!(
            " {} ",
            display_label(&tab.id.resource, tab.label.as_deref())
        ));
        let right = left.saturating_add(1 + title.width() + 1);
        if (left..right).contains(&column) {
            return Some(tab.id.clone());
        }
        left = right.saturating_add(usize::from(index + 1 < tabs.len()));
    }
    None
}

fn sidebar_block() -> Block<'static> {
    Block::default()
        .title(Span::styled(" super-herdr ", Style::default().bold()))
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray))
}

fn attention_sidebar_block(attention: &AttentionIndex) -> Block<'static> {
    let title = if attention.unread_count() > 0 {
        format!(" attention · {} new ", attention.unread_count())
    } else {
        " attention ".to_owned()
    };
    Block::default()
        .title(Span::styled(title, Style::default().bold()))
        .borders(Borders::TOP | Borders::RIGHT)
        .border_style(Style::default().fg(if attention.unread_count() > 0 {
            Color::Red
        } else {
            Color::DarkGray
        }))
}

fn sidebar_areas(area: Rect) -> (Rect, Rect) {
    let [sessions, attention] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    (sessions, attention)
}

fn sidebar_content_height(area: Rect) -> usize {
    let (sessions, _) = sidebar_areas(area);
    usize::from(sidebar_block().inner(sessions).height)
}

fn attention_sidebar_content_height(area: Rect, attention: &AttentionIndex) -> usize {
    let (_, attention_area) = sidebar_areas(area);
    usize::from(
        attention_sidebar_block(attention)
            .inner(attention_area)
            .height,
    )
}

fn sidebar_max_offset(row_count: usize, area: Rect) -> usize {
    row_count.saturating_sub(sidebar_content_height(area))
}

fn scroll_session_sidebar(
    state: &FederationState,
    app: &mut App,
    area: Rect,
    direction: TerminalScrollDirection,
) {
    let row_count = sidebar_session_rows(state, app.selected_pane.as_ref()).len();
    let maximum = sidebar_max_offset(row_count, area);
    let amount = usize::from(MOUSE_SCROLL_LINES);
    let next_offset = match direction {
        TerminalScrollDirection::Up => app.sidebar_offset.saturating_sub(amount),
        TerminalScrollDirection::Down => app.sidebar_offset.saturating_add(amount).min(maximum),
    };
    if next_offset != app.sidebar_offset {
        app.sidebar_offset = next_offset;
        app.sidebar_follow_selected = false;
    }
}

fn scroll_attention_sidebar(
    state: &FederationState,
    app: &mut App,
    area: Rect,
    direction: TerminalScrollDirection,
) {
    let row_count = sidebar_attention_rows(state, app.selected_pane.as_ref(), &app.attention).len();
    let maximum = row_count.saturating_sub(attention_sidebar_content_height(area, &app.attention));
    let amount = usize::from(MOUSE_SCROLL_LINES);
    app.attention_sidebar_offset = match direction {
        TerminalScrollDirection::Up => app.attention_sidebar_offset.saturating_sub(amount),
        TerminalScrollDirection::Down => app
            .attention_sidebar_offset
            .saturating_add(amount)
            .min(maximum),
    };
}

fn reconcile_sidebar_viewport(rows: &[SidebarRow], app: &mut App, area: Rect) {
    if app.sidebar_last_selected.as_ref() != app.selected_pane.as_ref() {
        app.sidebar_follow_selected = true;
        app.sidebar_last_selected = app.selected_pane.clone();
    }

    let height = sidebar_content_height(area);
    let maximum = rows.len().saturating_sub(height);
    app.sidebar_offset = app.sidebar_offset.min(maximum);
    if height == 0 || !app.sidebar_follow_selected {
        return;
    }

    let Some(selected_row) = rows.iter().position(|row| row.selection_anchor) else {
        return;
    };
    if selected_row < app.sidebar_offset {
        app.sidebar_offset = selected_row;
    } else if selected_row >= app.sidebar_offset.saturating_add(height) {
        app.sidebar_offset = selected_row.saturating_add(1).saturating_sub(height);
    }
    app.sidebar_offset = app.sidebar_offset.min(maximum);
}

fn update_sidebar_hit_areas(state: &FederationState, app: &mut App, area: Rect) {
    app.sidebar_hit_areas.clear();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (sessions_area, attention_area) = sidebar_areas(area);
    let sessions_content = sidebar_block().inner(sessions_area);
    let session_rows = sidebar_session_rows(state, app.selected_pane.as_ref());
    reconcile_sidebar_viewport(&session_rows, app, area);
    append_sidebar_hit_areas(
        &mut app.sidebar_hit_areas,
        session_rows,
        app.sidebar_offset,
        sessions_content,
    );

    let attention_content = attention_sidebar_block(&app.attention).inner(attention_area);
    let attention_rows = sidebar_attention_rows(state, app.selected_pane.as_ref(), &app.attention);
    app.attention_sidebar_offset = app.attention_sidebar_offset.min(
        attention_rows
            .len()
            .saturating_sub(usize::from(attention_content.height)),
    );
    append_sidebar_hit_areas(
        &mut app.sidebar_hit_areas,
        attention_rows,
        app.attention_sidebar_offset,
        attention_content,
    );
}

fn append_sidebar_hit_areas(
    hit_areas: &mut Vec<SidebarHitArea>,
    rows: Vec<SidebarRow>,
    offset: usize,
    content: Rect,
) {
    for (visible_offset, row) in rows
        .into_iter()
        .skip(offset)
        .take(usize::from(content.height))
        .enumerate()
    {
        let Ok(visible_offset) = u16::try_from(visible_offset) else {
            break;
        };
        if visible_offset >= content.height {
            break;
        }
        if row.pane.is_some() || row.context.is_some() {
            hit_areas.push(SidebarHitArea {
                area: Rect::new(content.x, content.y + visible_offset, content.width, 1),
                pane: row.pane,
                context: row.context,
            });
        }
    }
}

fn render_sidebar(frame: &mut Frame, state: &FederationState, app: &App, area: Rect) {
    let (sessions_area, attention_area) = sidebar_areas(area);
    let session_lines = sidebar_session_rows(state, app.selected_pane.as_ref())
        .into_iter()
        .map(|row| row.line)
        .collect::<Vec<_>>();
    let session_row_count = session_lines.len();
    let session_offset = u16::try_from(app.sidebar_offset).unwrap_or(u16::MAX);
    frame.render_widget(
        Paragraph::new(session_lines)
            .scroll((session_offset, 0))
            .block(sidebar_block()),
        sessions_area,
    );
    render_sidebar_scrollbar(
        frame,
        sessions_area,
        session_row_count,
        sidebar_content_height(area),
        app.sidebar_offset,
        Color::Blue,
    );

    let attention_lines = sidebar_attention_rows(state, app.selected_pane.as_ref(), &app.attention)
        .into_iter()
        .map(|row| row.line)
        .collect::<Vec<_>>();
    let attention_row_count = attention_lines.len();
    let attention_offset = u16::try_from(app.attention_sidebar_offset).unwrap_or(u16::MAX);
    frame.render_widget(
        Paragraph::new(attention_lines)
            .scroll((attention_offset, 0))
            .block(attention_sidebar_block(&app.attention)),
        attention_area,
    );
    render_sidebar_scrollbar(
        frame,
        attention_area,
        attention_row_count,
        attention_sidebar_content_height(area, &app.attention),
        app.attention_sidebar_offset,
        Color::Red,
    );
}

fn render_sidebar_scrollbar(
    frame: &mut Frame,
    area: Rect,
    row_count: usize,
    content_height: usize,
    offset: usize,
    color: Color,
) {
    if row_count <= content_height || row_count == 0 {
        return;
    }
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"))
        .track_symbol(Some("│"))
        .thumb_symbol("█")
        .track_style(Style::default().fg(Color::DarkGray))
        .thumb_style(Style::default().fg(color));
    let mut scrollbar_state = ScrollbarState::new(row_count).position(offset.min(row_count - 1));
    frame.render_stateful_widget(
        scrollbar,
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut scrollbar_state,
    );
}

fn render_tabs(frame: &mut Frame, state: &FederationState, app: &App, area: Rect) {
    let Some((snapshot, pane)) = selected_snapshot_and_pane(state, app.selected_pane.as_ref())
    else {
        frame.render_widget(Paragraph::new(" no live pane"), area);
        return;
    };
    let Some(workspace) = pane.workspace.as_ref() else {
        frame.render_widget(Paragraph::new(" pane has no workspace"), area);
        return;
    };
    let tabs = snapshot
        .tabs
        .values()
        .filter(|tab| tab.workspace.as_ref() == Some(workspace))
        .collect::<Vec<_>>();
    let selected = tabs
        .iter()
        .position(|tab| pane.tab.as_ref() == Some(&tab.id))
        .unwrap_or(0);
    let titles = tabs
        .iter()
        .map(|tab| {
            Line::from(format!(
                " {} ",
                display_label(&tab.id.resource, tab.label.as_deref())
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Tabs::new(titles)
            .select(selected)
            .style(Style::default().fg(Color::DarkGray))
            .highlight_style(Style::default().fg(Color::White).bold())
            .divider(Span::styled("│", Style::default().fg(Color::DarkGray))),
        area,
    );
}

fn render_terminal_surfaces(frame: &mut Frame, state: &FederationState, app: &App, area: Rect) {
    let displayed_message = displayed_message(app);
    let panes = visible_pane_areas(state, app.selected_pane.as_ref(), area);
    if panes.is_empty() {
        frame.render_widget(
            Paragraph::new(" waiting for a live terminal route")
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    let mut selected_inner = None;
    for (pane, pane_area) in panes {
        let selected = app.selected_pane.as_ref() == Some(&pane);
        let block = pane_block(
            &pane,
            selected,
            app.routes.get(&pane).map(|route| route.access),
        );
        let inner = block.inner(pane_area);
        frame.render_widget(block, pane_area);
        if selected {
            selected_inner = Some(inner);
        }
        if let Some(route) = app.routes.get(&pane) {
            render_vt_screen(
                frame,
                route.parser.screen(),
                inner,
                selected && app.mode == InputMode::Terminal,
                app.selection
                    .as_ref()
                    .filter(|selection| selection.pane == pane && selection.dragging),
            );
        } else if selected {
            let message = displayed_message.unwrap_or("waiting for a live terminal route");
            frame.render_widget(
                Paragraph::new(safe_text(message)).style(Style::default().fg(Color::DarkGray)),
                inner,
            );
        }
    }

    if let Some(inner) = selected_inner
        && inner.height > 0
        && (matches!(app.mode, InputMode::Prefix | InputMode::HerdrPrefix)
            || displayed_message.is_some())
    {
        let overlay = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        );
        let (text, style) = match app.mode {
            InputMode::Prefix => (
                " Ctrl+]  Space actions  1-9 workspace  p/n tab  j/k pane  d close  a agents  h hosts  v paste  i image  P pair  q quit "
                    .to_owned(),
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ),
            InputMode::HerdrPrefix => (
                " Herdr Ctrl+B  h/j/k/l pane  p/n tab  1-9 tab  c new tab  v/- split  z zoom  ? help "
                    .to_owned(),
                Style::default().fg(Color::Black).bg(Color::Cyan),
            ),
            InputMode::Terminal => (
                format!(" {} ", safe_text(displayed_message.unwrap_or_default())),
                Style::default().fg(Color::Black).bg(Color::DarkGray),
            ),
        };
        frame.render_widget(Paragraph::new(text).style(style), overlay);
    }
}

fn displayed_message(app: &App) -> Option<&str> {
    app.clipboard_feedback
        .as_ref()
        .filter(|feedback| feedback.expires_at > Instant::now())
        .map(|feedback| feedback.text.as_str())
        .or(app.message.as_deref())
}

fn render_vt_screen(
    frame: &mut Frame,
    screen: &vt100::Screen,
    area: Rect,
    show_cursor: bool,
    selection: Option<&TerminalSelection>,
) {
    let buffer = frame.buffer_mut();
    for row in 0..area.height {
        for column in 0..area.width {
            let Some(cell) = screen.cell(row, column) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let symbol = if cell.has_contents() {
                cell.contents()
            } else {
                " "
            };
            let mut foreground = vt_color(cell.fgcolor());
            let mut background = vt_color(cell.bgcolor());
            if cell.inverse() {
                std::mem::swap(&mut foreground, &mut background);
            }
            let mut style = Style::default().fg(foreground).bg(background);
            if cell.bold() {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.dim() {
                style = style.add_modifier(Modifier::DIM);
            }
            if cell.italic() {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if selection.is_some_and(|selection| {
                selection.contains(CellPosition::from_viewport(
                    row,
                    column,
                    selection.viewport_offset,
                ))
            }) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            buffer[(area.x + column, area.y + row)]
                .set_symbol(symbol)
                .set_style(style);
        }
    }
    if show_cursor && !screen.hide_cursor() {
        let (row, column) = screen.cursor_position();
        if row < area.height && column < area.width {
            frame.set_cursor_position(Position::new(area.x + column, area.y + row));
        }
    }
}

fn vt_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

fn selected_snapshot_and_pane<'a>(
    state: &'a FederationState,
    selected: Option<&PaneId>,
) -> Option<(&'a NormalizedSnapshot, &'a crate::state::PaneState)> {
    let selected = selected?;
    let snapshot = state
        .targets
        .get(&selected.target_session())?
        .snapshot
        .as_deref()?;
    let pane = snapshot.panes.get(selected)?;
    Some((snapshot, pane))
}

fn selected_workspace(state: &FederationState, selected: Option<&PaneId>) -> Option<WorkspaceId> {
    selected_snapshot_and_pane(state, selected).and_then(|(_, pane)| pane.workspace.clone())
}

fn target_status(target: &TargetRuntimeState) -> (&'static str, Color) {
    match target.connection {
        TargetConnectionState::Connecting => ("◌", Color::Yellow),
        TargetConnectionState::Live => ("●", Color::Green),
        TargetConnectionState::Backoff { .. } => ("!", Color::Red),
        TargetConnectionState::Incompatible => ("×", Color::Red),
    }
}

fn status_symbol(status: Option<&str>) -> &'static str {
    match status {
        Some("working") => "●",
        Some("blocked") => "!",
        Some("idle") => "✓",
        _ => "·",
    }
}

fn status_style(status: Option<&str>) -> Style {
    let color = match status {
        Some("working") => Color::Yellow,
        Some("blocked") => Color::Red,
        Some("idle") => Color::Green,
        _ => Color::DarkGray,
    };
    Style::default().fg(color)
}

fn display_label(id: &str, label: Option<&str>) -> String {
    label
        .filter(|label| !label.is_empty())
        .map(safe_text)
        .unwrap_or_else(|| safe_text(id))
}

fn safe_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn short_session(session: &str) -> String {
    const LIMIT: usize = 14;
    let safe = safe_text(session);
    if safe.chars().count() <= LIMIT {
        return safe;
    }
    safe.chars().take(LIMIT - 1).chain(['…']).collect()
}

fn federation_routes_changed(current: &Config, next: &Config) -> bool {
    current.transport != next.transport || current.targets != next.targets
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// A target whose socket comes from discovery can still take an atomic
    /// paste.
    ///
    /// The regression this is here for: the check used to read the file rather
    /// than what discovery resolved, so a target configured with
    /// `discover_sessions` and no `socket` line was told it could not do the
    /// one thing it could do — and a multiline paste into a pane without
    /// bracketed paste was refused with a reason that was not true.
    #[test]
    fn a_discovered_socket_is_enough_for_an_atomic_paste() {
        let configured = Target {
            name: "ws01".to_owned(),
            ssh: None,
            discover_sessions: true,
            session: Some("rv32sim".to_owned()),
            // What the file says: nothing. The host reports it at runtime.
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
        };
        let key = TargetSession::new("ws01", "rv32sim");

        // Before discovery has answered there is genuinely nothing to use.
        let unresolved = super::resolved_targets(&Config {
            transport: Default::default(),
            notifications: Default::default(),
            transfers: Default::default(),
            targets: vec![configured.clone()],
            devices: Vec::new(),
        });
        assert!(!super::atomic_paste_available(&unresolved, &key));

        // After it has, the socket exists and the paste is available.
        let discovered = Target {
            discover_sessions: false,
            socket: Some("/home/veba/.config/herdr/sessions/rv32sim/herdr.sock".to_owned()),
            ..configured
        };
        let resolved = super::resolved_targets(&Config {
            transport: Default::default(),
            notifications: Default::default(),
            transfers: Default::default(),
            targets: vec![discovered],
            devices: Vec::new(),
        });
        assert!(
            super::atomic_paste_available(&resolved, &key),
            "a discovered socket must count; reading the file instead is what broke this"
        );
    }

    use ratatui::style::Modifier;
    use serde_json::json;

    use super::{
        AgentFilter, AgentNavigator, App, AttentionCenter, AttentionIndex, CellPosition,
        ClipboardFeedback, ContextTarget, DecodedInput, HERDR_PREFIX_KEY, InputDecoder, InputMode,
        MAX_CLIPBOARD_BYTES, MAX_CONTEXT_MENU_MOVE_DESTINATIONS, MOUSE_CAPTURE_ENABLE, MouseInput,
        PREFIX_KEY, PaneDirection, SIDEBAR_WIDTH, SelectionAutoscroll,
        SelectionAutoscrollDirection, SelectionFinish, TargetForm, TargetManager,
        TargetManagerControl, TargetManagerMode, TargetTestEvent, TargetTestState,
        TerminalSelection, agent_jump_entries, capture_screen_rows, capture_selection_viewport,
        clamped_pane_position, command_palette_actions, context_menu_for_target, cycle_tab,
        desired_access, displayed_message, encode_mouse_event, federation_routes_changed,
        filtered_palette_actions, finish_ui_left_gesture, fuzzy_score,
        handle_attention_center_input, handle_context_menu_input, handle_decoded_input,
        handle_input, handle_mouse, handle_target_manager_input, handle_target_manager_mouse,
        jump_from_notification, mouse_event_allowed, mouse_passthrough_enabled,
        pane_direction_score, reconcile_selection, refresh_attention, render_vt_screen, safe_text,
        select_neighbor_pane, select_pane, select_workspace, selected_terminal_text, sidebar_areas,
        sidebar_attention_rows, sidebar_block, sidebar_content_height, sidebar_pane_at_row,
        tab_at_column, target_manager_controls, terminal_paste_payload, ui_areas,
        update_selection_after_frame, update_sidebar_hit_areas, viewport_shift_distance,
        visible_pane_areas,
    };
    use crate::config::{Config, Target};
    use crate::model::{PaneId, TargetSession, WorkspaceId};
    use crate::resource_action::ResourceAction;
    use crate::state::{
        FederationState, NormalizedSnapshot, TargetConnectionState, TargetRuntimeState,
    };
    use crate::terminal::{TerminalAccess, TerminalScrollDirection};
    use crate::transport::{DiscoveredSession, SessionDiscovery};

    /// Derive attention the way the daemon does, then hand it to the frontend
    /// the way the wire does. A frontend cannot reach `observe` any more, and a
    /// test that called it directly would be testing a path that no longer
    /// exists.
    fn mirror_attention(app: &mut App, states: &[&FederationState]) {
        let mut index = AttentionIndex::default();
        for state in states {
            index.observe(state);
        }
        app.attention = AttentionIndex::mirror(index.events().cloned().collect());
        refresh_attention(app);
    }

    #[test]
    fn desktop_layout_reserves_most_space_for_the_terminal() {
        let (sidebar, tabs, terminal) = ui_areas(ratatui::layout::Rect::new(0, 0, 120, 40));

        assert_eq!(sidebar.width, SIDEBAR_WIDTH);
        assert_eq!(tabs.height, 1);
        assert_eq!(terminal.width, 120 - SIDEBAR_WIDTH);
        assert_eq!(terminal.height, 39);
    }

    #[test]
    fn narrow_layout_gives_the_terminal_full_width() {
        let (sidebar, _, terminal) = ui_areas(ratatui::layout::Rect::new(0, 0, 60, 20));

        assert_eq!(sidebar.width, 0);
        assert_eq!(terminal.width, 60);
    }

    #[test]
    fn attention_feed_occupies_the_lower_half_of_the_sidebar() {
        let area = ratatui::layout::Rect::new(0, 0, SIDEBAR_WIDTH, 40);
        let (sessions, attention) = sidebar_areas(area);

        assert_eq!(
            sessions,
            ratatui::layout::Rect::new(0, 0, SIDEBAR_WIDTH, 20)
        );
        assert_eq!(
            attention,
            ratatui::layout::Rect::new(0, 20, SIDEBAR_WIDTH, 20)
        );
    }

    #[test]
    fn federation_prefix_does_not_shadow_herdrs_prefix() {
        assert_eq!(PREFIX_KEY, 0x1d);
        assert_eq!(HERDR_PREFIX_KEY, 0x02);
        assert_ne!(PREFIX_KEY, HERDR_PREFIX_KEY);
    }

    #[test]
    fn notification_refresh_does_not_rebuild_terminal_routes() {
        let current = Config::parse(
            r#"
                [[targets]]
                name = "host-a"
            "#,
        )
        .unwrap();
        let mut notifications_changed = current.clone();
        notifications_changed.notifications.enabled = true;
        assert!(!federation_routes_changed(&current, &notifications_changed));

        let mut targets_changed = current.clone();
        targets_changed.targets[0].session = Some("work".to_owned());
        assert!(federation_routes_changed(&current, &targets_changed));
    }

    #[tokio::test]
    async fn ctrl_b_enters_herdr_action_mode_instead_of_the_raw_terminal() {
        let mut app = App::default();
        let state = FederationState::default();
        let transport = crate::config::TransportConfig::default();

        handle_input(HERDR_PREFIX_KEY, &state, &transport, &mut app)
            .await
            .unwrap();

        assert_eq!(app.mode, InputMode::HerdrPrefix);
        assert!(app.routes.is_empty());

        handle_input(b'?', &state, &transport, &mut app)
            .await
            .unwrap();
        assert_eq!(app.mode, InputMode::Terminal);
        assert!(app.message.as_deref().unwrap().contains("Herdr Ctrl+B"));
    }

    #[tokio::test]
    async fn federation_prefix_qualifies_workspace_close_before_confirmation() {
        let first_key = TargetSession::new("host-a", "work");
        let second_key = TargetSession::new("host-b", "work");
        let snapshot = |key: &TargetSession, label: &str| {
            NormalizedSnapshot::from_value(
                key,
                &json!({
                    "workspaces": [{
                        "workspace_id": "w1",
                        "active_tab_id": "w1:t1",
                        "label": label
                    }],
                    "tabs": [{"tab_id": "w1:t1", "workspace_id": "w1"}],
                    "panes": [{
                        "pane_id": "w1:p1",
                        "workspace_id": "w1",
                        "tab_id": "w1:t1"
                    }]
                }),
            )
        };
        let mut state = FederationState::default();
        state.targets.insert(
            first_key.clone(),
            runtime(
                first_key.clone(),
                TargetConnectionState::Live,
                Some(snapshot(&first_key, "first")),
            ),
        );
        state.targets.insert(
            second_key.clone(),
            runtime(
                second_key.clone(),
                TargetConnectionState::Live,
                Some(snapshot(&second_key, "second")),
            ),
        );
        let transport = crate::config::TransportConfig::default();
        let mut app = App {
            selected_pane: Some(PaneId::new("host-b", "work", "w1:p1")),
            ..App::default()
        };

        handle_input(PREFIX_KEY, &state, &transport, &mut app)
            .await
            .unwrap();
        handle_input(b'd', &state, &transport, &mut app)
            .await
            .unwrap();

        let confirmation = app.close_confirmation.as_ref().unwrap();
        let ResourceAction::CloseWorkspace { workspace, label } = &confirmation.action else {
            panic!("expected a workspace-close action");
        };
        assert_eq!(workspace.to_string(), "host-b/work/w1");
        assert_eq!(label, "second");
        assert_eq!(app.mode, InputMode::Terminal);

        handle_input(b'n', &state, &transport, &mut app)
            .await
            .unwrap();
        assert!(app.close_confirmation.is_none());
        assert!(!app.herdr_action_inflight);

        let actions = filtered_palette_actions(&state, app.selected_pane.as_ref(), "jump second");
        assert_eq!(actions.len(), 1);
        let ResourceAction::JumpToPane { pane, .. } = &actions[0] else {
            panic!("expected a jump action");
        };
        assert_eq!(pane.to_string(), "host-b/work/w1:p1");
    }

    #[test]
    fn command_palette_search_is_case_insensitive_and_fuzzy() {
        assert!(fuzzy_score("Close workspace Simulator", "CWSim").is_some());
        assert!(fuzzy_score("Open agent navigator", "target remove").is_none());
    }

    #[test]
    fn workspace_context_menu_keeps_its_resource_qualified() {
        let key = TargetSession::new("host-b", "work");
        let workspace = WorkspaceId::new("host-b", "work", "w2");
        let snapshot = NormalizedSnapshot::from_value(
            &key,
            &json!({
                "workspaces": [{"workspace_id": "w2", "label": "simulator"}],
                "tabs": [{"tab_id": "w2:t1", "workspace_id": "w2"}],
                "panes": [{"pane_id": "w2:p1", "workspace_id": "w2", "tab_id": "w2:t1"}]
            }),
        );
        let mut state = FederationState::default();
        state.targets.insert(
            key.clone(),
            runtime(key, TargetConnectionState::Live, Some(snapshot)),
        );

        let menu = context_menu_for_target(
            &state,
            ContextTarget::Workspace(workspace.clone()),
            ratatui::layout::Position::new(4, 5),
        )
        .unwrap();

        assert!(menu.title.contains("host-b/work"));
        assert!(menu.actions.iter().all(|action| {
            action.target_session() == Some(TargetSession::new("host-b", "work"))
        }));
        assert!(menu.actions.contains(&ResourceAction::CloseWorkspace {
            workspace,
            label: "simulator".to_owned(),
        }));
    }

    #[test]
    fn workspace_moves_are_offered_only_inside_one_session() {
        let key = TargetSession::new("host-b", "work");
        let other = TargetSession::new("host-c", "build");
        let workspace = WorkspaceId::new("host-b", "work", "w2");
        let snapshot = NormalizedSnapshot::from_value(
            &key,
            &json!({
                "workspaces": [
                    {"workspace_id": "w2", "label": "simulator"},
                    {"workspace_id": "w3", "label": "toolchain"}
                ],
                "tabs": [{"tab_id": "w2:t1", "workspace_id": "w2"}],
                "panes": [{"pane_id": "w2:p1", "workspace_id": "w2", "tab_id": "w2:t1"}]
            }),
        );
        let other_snapshot = NormalizedSnapshot::from_value(
            &other,
            &json!({
                "workspaces": [{"workspace_id": "w1", "label": "remote"}],
                "tabs": [{"tab_id": "w1:t1", "workspace_id": "w1"}],
                "panes": [{"pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1"}]
            }),
        );
        let mut state = FederationState::default();
        state.targets.insert(
            key.clone(),
            runtime(key.clone(), TargetConnectionState::Live, Some(snapshot)),
        );
        state.targets.insert(
            other.clone(),
            runtime(other, TargetConnectionState::Live, Some(other_snapshot)),
        );

        let menu = context_menu_for_target(
            &state,
            ContextTarget::Workspace(workspace.clone()),
            ratatui::layout::Position::new(4, 5),
        )
        .unwrap();

        let moves = menu
            .actions
            .iter()
            .filter(|action| matches!(action, ResourceAction::MoveWorkspace { .. }))
            .collect::<Vec<_>>();
        assert_eq!(
            moves,
            vec![&ResourceAction::MoveWorkspace {
                workspace,
                destination: WorkspaceId::new("host-b", "work", "w3"),
                label: "simulator".to_owned(),
                destination_label: "toolchain".to_owned(),
            }]
        );
        assert!(matches!(
            menu.actions.last(),
            Some(ResourceAction::CloseWorkspace { .. })
        ));
    }

    #[test]
    fn a_clicked_notification_jumps_only_while_its_pane_is_live() {
        let key = TargetSession::new("host-a", "work");
        let pane = PaneId::new("host-a", "work", "w1:p1");
        let state_with_status = |status: &str| {
            let snapshot = NormalizedSnapshot::from_value(
                &key,
                &json!({
                    "workspaces": [{"workspace_id": "w1", "label": "compiler"}],
                    "panes": [{
                        "pane_id": "w1:p1",
                        "workspace_id": "w1",
                        "agent": "builder",
                        "agent_status": status
                    }],
                    "agents": [{
                        "pane_id": "w1:p1",
                        "name": "builder",
                        "agent_status": status
                    }]
                }),
            );
            let mut state = FederationState::default();
            state.targets.insert(
                key.clone(),
                runtime(key.clone(), TargetConnectionState::Live, Some(snapshot)),
            );
            state
        };
        let working = state_with_status("working");
        let live = state_with_status("blocked");
        let notify = |app: &mut App| mirror_attention(app, &[&working, &live]);
        let mut app = App::default();
        notify(&mut app);
        assert_eq!(app.attention.unread_count(), 1);

        jump_from_notification(&live, &mut app, pane.clone());
        assert_eq!(app.selected_pane, Some(pane.clone()));
        assert!(displayed_message(&app).unwrap().contains("host-a/work"));

        // The identifier is only as fresh as the notification that carried it.
        let mut app = App::default();
        notify(&mut app);
        let gone = FederationState::default();
        jump_from_notification(&gone, &mut app, pane);
        assert_eq!(app.selected_pane, None);
        assert_eq!(app.attention.unread_count(), 0);
        assert!(displayed_message(&app).unwrap().contains("no longer live"));
    }

    #[test]
    fn recreation_targets_other_live_sessions_and_never_the_source() {
        let key = TargetSession::new("host-b", "work");
        let same_host = TargetSession::new("host-b", "scratch");
        let other_host = TargetSession::new("host-c", "build");
        let offline = TargetSession::new("host-d", "spare");
        let workspace = WorkspaceId::new("host-b", "work", "w2");
        let snapshot = |target: &TargetSession, resource: &str| {
            NormalizedSnapshot::from_value(
                target,
                &json!({
                    "workspaces": [{"workspace_id": resource, "label": "simulator"}],
                    "tabs": [{"tab_id": format!("{resource}:t1"), "workspace_id": resource}],
                    "panes": [{
                        "pane_id": format!("{resource}:p1"),
                        "workspace_id": resource,
                        "tab_id": format!("{resource}:t1"),
                    }]
                }),
            )
        };
        let mut state = FederationState::default();
        for (target, connection, resource) in [
            (&key, TargetConnectionState::Live, "w2"),
            (&same_host, TargetConnectionState::Live, "w1"),
            (&other_host, TargetConnectionState::Live, "w1"),
        ] {
            state.targets.insert(
                target.clone(),
                runtime(target.clone(), connection, Some(snapshot(target, resource))),
            );
        }
        state.targets.insert(
            offline.clone(),
            runtime(offline, TargetConnectionState::Backoff { attempt: 2 }, None),
        );

        let menu = context_menu_for_target(
            &state,
            ContextTarget::Workspace(workspace.clone()),
            ratatui::layout::Position::new(4, 5),
        )
        .unwrap();

        let destinations = menu
            .actions
            .iter()
            .filter_map(|action| match action {
                ResourceAction::RecreateWorkspace { destination, .. } => Some(destination.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(destinations, vec![same_host, other_host]);
        assert!(menu.actions.iter().all(|action| {
            action.target_session() == Some(TargetSession::new("host-b", "work"))
        }));
        assert!(
            menu.actions
                .iter()
                .any(|action| action.palette_label().contains("new shells"))
        );
    }

    #[test]
    fn a_crowded_session_caps_menu_destinations_but_not_the_palette() {
        let key = TargetSession::new("host-b", "work");
        let workspaces = (1..=12)
            .map(|index| json!({"workspace_id": format!("w{index}")}))
            .collect::<Vec<_>>();
        let snapshot = NormalizedSnapshot::from_value(
            &key,
            &json!({
                "workspaces": workspaces,
                "tabs": [{"tab_id": "w1:t1", "workspace_id": "w1"}],
                "panes": [{"pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1"}]
            }),
        );
        let mut state = FederationState::default();
        state.targets.insert(
            key.clone(),
            runtime(key.clone(), TargetConnectionState::Live, Some(snapshot)),
        );

        let menu = context_menu_for_target(
            &state,
            ContextTarget::Workspace(WorkspaceId::new("host-b", "work", "w1")),
            ratatui::layout::Position::new(0, 0),
        )
        .unwrap();
        let menu_moves = menu
            .actions
            .iter()
            .filter(|action| matches!(action, ResourceAction::MoveWorkspace { .. }))
            .count();
        assert_eq!(menu_moves, MAX_CONTEXT_MENU_MOVE_DESTINATIONS);

        let palette =
            command_palette_actions(&state, Some(&PaneId::new("host-b", "work", "w1:p1")));
        let palette_moves = palette
            .iter()
            .filter(|action| matches!(action, ResourceAction::MoveWorkspace { .. }))
            .count();
        assert_eq!(palette_moves, 11);
    }

    #[tokio::test]
    async fn command_palette_consumes_arrow_navigation_as_one_input() {
        let state = FederationState::default();
        let transport = crate::config::TransportConfig::default();
        let mut app = App::default();

        handle_input(PREFIX_KEY, &state, &transport, &mut app)
            .await
            .unwrap();
        handle_input(b' ', &state, &transport, &mut app)
            .await
            .unwrap();
        handle_decoded_input(
            DecodedInput::Bytes(b"\x1b[B".to_vec()),
            &state,
            &transport,
            &mut app,
        )
        .await
        .unwrap();

        assert_eq!(app.command_palette.as_ref().unwrap().selected, 1);
        assert!(app.message.is_none());
    }

    #[test]
    fn ctrl_b_directional_navigation_uses_the_server_layout() {
        let key = TargetSession::new("host-a", "work");
        let left = PaneId::new("host-a", "work", "w1:p1");
        let right = PaneId::new("host-a", "work", "w1:p2");
        let snapshot = NormalizedSnapshot::from_value(
            &key,
            &json!({
                "panes": [
                    {"pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1"},
                    {"pane_id": "w1:p2", "workspace_id": "w1", "tab_id": "w1:t1"}
                ],
                "layouts": [layout_with_panes(
                    "w1",
                    "w1:t1",
                    "w1:p1",
                    &[("w1:p1", 0, 0, 40, 20), ("w1:p2", 40, 0, 40, 20)]
                )]
            }),
        );
        let mut state = FederationState::default();
        state.targets.insert(
            key.clone(),
            runtime(key, TargetConnectionState::Live, Some(snapshot)),
        );
        let mut app = App {
            selected_pane: Some(left),
            ..App::default()
        };

        select_neighbor_pane(&state, &mut app, PaneDirection::Right);

        assert_eq!(app.selected_pane, Some(right));
        assert_eq!(
            pane_direction_score(
                crate::state::LayoutRect {
                    x: 0,
                    y: 0,
                    width: 40,
                    height: 20,
                },
                crate::state::LayoutRect {
                    x: 40,
                    y: 0,
                    width: 40,
                    height: 20,
                },
                PaneDirection::Right,
            ),
            Some((0, 0))
        );
    }

    #[test]
    fn target_manager_adds_a_host_through_the_shared_config_store() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let existing = Target {
            name: "existing".to_owned(),
            ssh: Some("existing-host".to_owned()),
            discover_sessions: true,
            session: None,
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
        };
        Config::add_target_file(Some(&path), existing.clone()).unwrap();
        let mut app = App {
            config_path: Some(path.clone()),
            configured_targets: vec![existing.clone()],
            target_manager: Some(TargetManager::new(vec![existing])),
            ..App::default()
        };

        let transport = crate::config::TransportConfig::default();
        handle_target_manager_input(b'a', &transport, &mut app).unwrap();
        for byte in b"build" {
            handle_target_manager_input(*byte, &transport, &mut app).unwrap();
        }
        handle_target_manager_input(b'\t', &transport, &mut app).unwrap();
        for byte in b"build-host" {
            handle_target_manager_input(*byte, &transport, &mut app).unwrap();
        }
        handle_target_manager_input(b'\r', &transport, &mut app).unwrap();

        let config = Config::load(Some(&path)).unwrap().0;
        assert_eq!(config.targets.len(), 2);
        assert_eq!(config.targets[1].name, "build");
        assert_eq!(config.targets[1].endpoint(), "build-host");
        assert!(config.targets[1].discover_sessions);
        assert!(app.configuration_dirty);
        assert!(!fs::metadata(path).unwrap().permissions().readonly());
    }

    #[test]
    fn target_manager_validates_before_writing_and_applies_discovered_sessions() {
        let existing = Target {
            name: "existing".to_owned(),
            ssh: Some("existing-host".to_owned()),
            discover_sessions: true,
            session: None,
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
        };
        let mut duplicate = TargetForm::add();
        duplicate.name = "existing".to_owned();
        assert!(
            duplicate
                .validation_error(std::slice::from_ref(&existing))
                .unwrap()
                .contains("duplicate target name")
        );

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::add_target_file(Some(&path), existing.clone()).unwrap();
        duplicate.ssh = "replacement-host".to_owned();
        let mut invalid_app = App {
            config_path: Some(path.clone()),
            configured_targets: vec![existing.clone()],
            target_manager: Some(TargetManager {
                targets: vec![existing.clone()],
                selected: 0,
                mode: TargetManagerMode::Form(Box::new(duplicate)),
                mouse_press: None,
            }),
            ..App::default()
        };
        handle_target_manager_input(
            b'\r',
            &crate::config::TransportConfig::default(),
            &mut invalid_app,
        )
        .unwrap();
        assert_eq!(Config::load(Some(&path)).unwrap().0.targets.len(), 1);
        let TargetManagerMode::Form(form) = &invalid_app.target_manager.as_ref().unwrap().mode
        else {
            panic!("invalid form should remain open");
        };
        assert!(
            form.error
                .as_deref()
                .unwrap()
                .contains("duplicate target name")
        );

        let mut form = TargetForm::edit(&existing);
        form.test_state = TargetTestState::Testing { request_id: 7 };
        let mut app = App {
            target_manager: Some(TargetManager {
                targets: vec![existing],
                selected: 0,
                mode: TargetManagerMode::Form(Box::new(form)),
                mouse_press: None,
            }),
            ..App::default()
        };
        super::apply_target_test_event(
            TargetTestEvent {
                request_id: 7,
                elapsed_ms: 12,
                result: Ok(SessionDiscovery {
                    sessions: vec![DiscoveredSession {
                        name: "build".to_owned(),
                        running: true,
                        session_dir: "/srv/herdr/build".to_owned(),
                        socket_path: "/srv/herdr/build/herdr.sock".to_owned(),
                    }],
                    herdr_bin: "herdr".to_owned(),
                }),
            },
            &mut app,
        );
        let manager = app.target_manager.as_mut().unwrap();
        let TargetManagerMode::Form(form) = &mut manager.mode else {
            panic!("expected target form");
        };
        assert!(matches!(form.test_state, TargetTestState::Passed { .. }));
        assert_eq!(form.discovered_sessions.len(), 1);
        form.select_discovered_session(0);
        assert_eq!(form.session, "build");
        assert_eq!(form.socket.as_deref(), Some("/srv/herdr/build/herdr.sock"));
        assert!(!form.discover_sessions);

        form.test_state = TargetTestState::Testing { request_id: 8 };
        form.invalidate_test();
        super::apply_target_test_event(
            TargetTestEvent {
                request_id: 8,
                elapsed_ms: 20,
                result: Ok(SessionDiscovery {
                    sessions: vec![DiscoveredSession {
                        name: "stale".to_owned(),
                        running: true,
                        session_dir: "/srv/herdr/stale".to_owned(),
                        socket_path: "/srv/herdr/stale/herdr.sock".to_owned(),
                    }],
                    herdr_bin: "herdr".to_owned(),
                }),
            },
            &mut app,
        );
        let TargetManagerMode::Form(form) = &app.target_manager.as_ref().unwrap().mode else {
            panic!("expected target form");
        };
        assert!(form.discovered_sessions.is_empty());
        assert_eq!(form.test_state, TargetTestState::Untested);
    }

    #[test]
    fn target_manager_mouse_uses_the_same_rendered_controls() {
        let target = Target {
            name: "development".to_owned(),
            ssh: Some("development-host".to_owned()),
            discover_sessions: true,
            session: None,
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
        };
        let frame_area = ratatui::layout::Rect::new(0, 0, 120, 40);
        let mut app = App {
            target_manager: Some(TargetManager::new(vec![target])),
            ..App::default()
        };
        let add_area = target_manager_controls(frame_area, app.target_manager.as_ref().unwrap())
            .into_iter()
            .find_map(|(area, control)| (control == TargetManagerControl::Add).then_some(area))
            .unwrap();
        let transport = crate::config::TransportConfig::default();
        for release in [false, true] {
            handle_target_manager_mouse(
                MouseInput {
                    code: 0,
                    column: add_area.x + 1,
                    row: add_area.y + 1,
                    release,
                },
                frame_area,
                &transport,
                &mut app,
            )
            .unwrap();
        }
        assert!(matches!(
            app.target_manager.as_ref().unwrap().mode,
            TargetManagerMode::Form(_)
        ));

        let ssh_area = target_manager_controls(frame_area, app.target_manager.as_ref().unwrap())
            .into_iter()
            .find_map(|(area, control)| {
                (control == TargetManagerControl::Field(super::TargetFormField::Ssh))
                    .then_some(area)
            })
            .unwrap();
        for release in [false, true] {
            handle_target_manager_mouse(
                MouseInput {
                    code: 0,
                    column: ssh_area.x + 1,
                    row: ssh_area.y + 1,
                    release,
                },
                frame_area,
                &transport,
                &mut app,
            )
            .unwrap();
        }
        let TargetManagerMode::Form(form) = &app.target_manager.as_ref().unwrap().mode else {
            panic!("expected target form");
        };
        assert_eq!(form.field, super::TargetFormField::Ssh);
    }

    #[test]
    fn agent_navigator_prioritizes_attention_and_filters_globally() {
        let key = TargetSession::new("host-a", "work");
        let snapshot = NormalizedSnapshot::from_value(
            &key,
            &json!({
                "workspaces": [
                    {"workspace_id": "w1", "name": "compiler"},
                    {"workspace_id": "w2", "name": "simulator"}
                ],
                "panes": [
                    {"pane_id": "w1:p1", "workspace_id": "w1", "agent": "builder", "agent_status": "working"},
                    {"pane_id": "w2:p1", "workspace_id": "w2", "agent": "tester", "agent_status": "blocked"},
                    {"pane_id": "w2:p2", "workspace_id": "w2", "agent": "reviewer", "agent_status": "idle"}
                ],
                "agents": [
                    {"pane_id": "w1:p1", "name": "builder", "agent_status": "working"},
                    {"pane_id": "w2:p1", "name": "tester", "agent_status": "blocked"},
                    {"pane_id": "w2:p2", "name": "reviewer", "agent_status": "idle"}
                ]
            }),
        );
        let mut state = FederationState::default();
        state.targets.insert(
            key.clone(),
            runtime(key, TargetConnectionState::Live, Some(snapshot)),
        );

        let all = agent_jump_entries(&state, AgentFilter::All);
        assert_eq!(
            all.iter()
                .map(|entry| entry.agent.as_str())
                .collect::<Vec<_>>(),
            ["tester", "builder", "reviewer"]
        );
        let attention = agent_jump_entries(&state, AgentFilter::Attention);
        assert_eq!(attention.len(), 1);
        assert_eq!(attention[0].agent, "tester");
        assert_eq!(AgentNavigator::default().filter, AgentFilter::Attention);
        let active = agent_jump_entries(&state, AgentFilter::Active);
        assert_eq!(
            active
                .iter()
                .map(|entry| entry.agent.as_str())
                .collect::<Vec<_>>(),
            ["tester", "builder"]
        );

        let rows = sidebar_attention_rows(&state, None, &AttentionIndex::default());
        assert_eq!(rows[0].line.to_string(), " waiting now (1)");
        assert_eq!(rows[1].pane, Some(PaneId::new("host-a", "work", "w2:p1")));
        assert!(rows[1].line.to_string().contains("tester"));
        assert!(rows[1].line.to_string().contains("host-a/work"));
        assert!(
            rows.iter()
                .take_while(|row| !row.line.to_string().starts_with(" history"))
                .all(|row| !row.line.to_string().contains("builder"))
        );
    }

    #[test]
    fn attention_history_is_unread_once_and_jumps_to_the_qualified_pane() {
        let key = TargetSession::new("host-a", "work");
        let state_with_status = |status: &str| {
            let snapshot = NormalizedSnapshot::from_value(
                &key,
                &json!({
                    "workspaces": [{"workspace_id": "w1", "label": "compiler"}],
                    "panes": [{
                        "pane_id": "w1:p1",
                        "workspace_id": "w1",
                        "agent": "builder",
                        "agent_status": status
                    }],
                    "agents": [{
                        "pane_id": "w1:p1",
                        "name": "builder",
                        "agent_status": status
                    }]
                }),
            );
            let mut state = FederationState::default();
            state.targets.insert(
                key.clone(),
                runtime(key.clone(), TargetConnectionState::Live, Some(snapshot)),
            );
            state
        };
        let working = state_with_status("working");
        let blocked = state_with_status("blocked");
        let mut app = App::default();

        mirror_attention(&mut app, &[&working, &blocked, &blocked]);
        assert_eq!(app.attention.events().count(), 1);
        assert_eq!(app.attention.unread_count(), 1);

        let rows = sidebar_attention_rows(&blocked, None, &app.attention);
        assert!(rows[0].line.to_string().contains("waiting now"));
        assert!(rows[1].line.to_string().contains("● builder"));
        assert!(
            rows.iter()
                .any(|row| { row.line.to_string().contains("history · 1 unread") })
        );
        let sidebar_area = ratatui::layout::Rect::new(0, 0, SIDEBAR_WIDTH, 30);
        update_sidebar_hit_areas(&blocked, &mut app, sidebar_area);
        let attention_area = sidebar_areas(sidebar_area).1;
        assert!(app.sidebar_hit_areas.iter().any(|hit| {
            hit.area.y >= attention_area.y
                && hit.pane.as_ref() == Some(&PaneId::new("host-a", "work", "w1:p1"))
        }));

        app.attention_center = Some(AttentionCenter::default());
        handle_attention_center_input(b'\r', &blocked, &mut app).unwrap();
        assert_eq!(
            app.selected_pane,
            Some(PaneId::new("host-a", "work", "w1:p1"))
        );
        assert_eq!(app.attention.unread_count(), 0);
        assert!(app.attention_center.is_none());
    }

    #[test]
    fn outer_mouse_capture_reports_drags_but_not_hover_motion() {
        assert!(MOUSE_CAPTURE_ENABLE.starts_with(b"\x1b[?1002h"));
        assert!(MOUSE_CAPTURE_ENABLE.ends_with(b"?1006h"));
        assert!(!MOUSE_CAPTURE_ENABLE.windows(6).any(|part| part == b"1003h"));
    }

    #[test]
    fn read_only_routes_fall_back_to_local_mouse_selection() {
        assert!(!mouse_passthrough_enabled(
            vt100::MouseProtocolMode::ButtonMotion,
            false,
        ));
        assert!(mouse_passthrough_enabled(
            vt100::MouseProtocolMode::ButtonMotion,
            true,
        ));
        assert!(!mouse_passthrough_enabled(
            vt100::MouseProtocolMode::None,
            true,
        ));
    }

    #[test]
    fn mouse_release_retains_a_completed_selection() {
        let mut selection = TerminalSelection {
            pane: PaneId::new("host-a", "work", "w6:p1"),
            anchor: CellPosition { row: 2, column: 3 },
            head: CellPosition { row: 2, column: 3 },
            viewport_offset: 0,
            captured_rows: BTreeMap::new(),
            dragging: false,
            forwarded_click: Some(MouseInput {
                code: 0,
                column: 4,
                row: 3,
                release: false,
            }),
        };
        let result = selection.finish(
            CellPosition { row: 2, column: 8 },
            CellPosition { row: 2, column: 8 },
            MouseInput {
                code: 0,
                column: 9,
                row: 3,
                release: true,
            },
        );

        assert_eq!(result, SelectionFinish::Retain);
        assert!(selection.dragging);
        assert!(selection.forwarded_click.is_none());
        assert!(selection.contains(CellPosition { row: 2, column: 6 }));
    }

    #[test]
    fn input_decoder_preserves_keys_and_extracts_sgr_mouse_events() {
        let mut decoder = InputDecoder::default();
        assert_eq!(decoder.push(0x1b), None);
        assert_eq!(decoder.push(b'['), None);
        assert_eq!(
            decoder.push(b'A'),
            Some(DecodedInput::Bytes(b"\x1b[A".to_vec()))
        );

        let mut decoded = None;
        for byte in b"\x1b[<64;42;9M" {
            decoded = decoder.push(*byte).or(decoded);
        }
        assert_eq!(
            decoded,
            Some(DecodedInput::Mouse(MouseInput {
                code: 64,
                column: 42,
                row: 9,
                release: false,
            }))
        );
    }

    #[test]
    fn input_decoder_emits_multiline_bracketed_paste_once() {
        let mut decoder = InputDecoder::default();
        let mut decoded = Vec::new();
        for byte in b"\x1b[200~first\nsecond\nthird\x1b[201~" {
            if let Some(input) = decoder.push(*byte) {
                decoded.push(input);
            }
        }

        assert_eq!(
            decoded,
            vec![DecodedInput::Paste(b"first\nsecond\nthird".to_vec())]
        );
    }

    #[test]
    fn input_decoder_discards_an_oversized_paste_as_one_event() {
        let mut decoder = InputDecoder::default();
        for byte in b"\x1b[200~" {
            assert_eq!(decoder.push(*byte), None);
        }
        for _ in 0..=MAX_CLIPBOARD_BYTES {
            assert_eq!(decoder.push(b'x'), None);
        }
        let mut decoded = None;
        for byte in b"\x1b[201~" {
            decoded = decoder.push(*byte).or(decoded);
        }

        assert_eq!(decoded, Some(DecodedInput::PasteTooLarge));
    }

    #[test]
    fn mouse_events_follow_the_inner_terminals_protocol() {
        let press = MouseInput {
            code: 0,
            column: 7,
            row: 3,
            release: false,
        };
        let motion = MouseInput { code: 32, ..press };
        assert!(!mouse_event_allowed(vt100::MouseProtocolMode::None, press));
        assert!(!mouse_event_allowed(
            vt100::MouseProtocolMode::PressRelease,
            motion
        ));
        assert!(mouse_event_allowed(
            vt100::MouseProtocolMode::ButtonMotion,
            motion
        ));
        assert_eq!(
            encode_mouse_event(press, vt100::MouseProtocolEncoding::Sgr),
            Some(b"\x1b[<0;7;3M".to_vec())
        );

        let modified_wheel = MouseInput {
            code: 64 | 4 | 16,
            ..press
        };
        assert_eq!(
            modified_wheel.scroll_direction(),
            Some(TerminalScrollDirection::Up)
        );
        assert_eq!(
            modified_wheel.key_modifiers(),
            (crossterm::event::KeyModifiers::SHIFT | crossterm::event::KeyModifiers::CONTROL)
                .bits()
        );
    }

    #[tokio::test]
    async fn right_click_opens_exact_pane_actions_and_preserves_close_confirmation() {
        let key = TargetSession::new("host-a", "dev");
        let pane = PaneId::new("host-a", "dev", "w1:p1");
        let snapshot = NormalizedSnapshot::from_value(
            &key,
            &json!({
                "workspaces": [{"workspace_id": "w1"}],
                "tabs": [{"tab_id": "w1:t1", "workspace_id": "w1"}],
                "panes": [{"pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1", "label": "shell"}],
                "layouts": [layout("w1", "w1:t1", "w1:p1")]
            }),
        );
        let mut state = FederationState::default();
        state.targets.insert(
            key.clone(),
            runtime(key, TargetConnectionState::Live, Some(snapshot)),
        );
        let frame_area = ratatui::layout::Rect::new(0, 0, 120, 40);
        let terminal_area = ui_areas(frame_area).2;
        let pane_area = visible_pane_areas(&state, Some(&pane), terminal_area)[0].1;
        let inner = super::pane_block(&pane, true, None).inner(pane_area);
        let mut app = App {
            selected_pane: Some(pane.clone()),
            last_frame_area: Some(frame_area),
            ..App::default()
        };
        let transport = crate::config::TransportConfig::default();

        handle_mouse(
            MouseInput {
                code: 2,
                column: inner.x + 1,
                row: inner.y + 1,
                release: false,
            },
            &state,
            &transport,
            &mut app,
        )
        .await
        .unwrap();

        let menu = app.context_menu.as_mut().unwrap();
        assert!(menu.title.contains("host-a/dev"));
        menu.selected = menu
            .actions
            .iter()
            .position(ResourceAction::is_destructive)
            .unwrap();
        handle_context_menu_input(b'\r', &state, &mut app).unwrap();
        assert_eq!(
            app.close_confirmation,
            Some(super::CloseConfirmation {
                action: ResourceAction::ClosePane {
                    pane,
                    label: "shell".to_owned(),
                },
            })
        );
    }

    #[test]
    fn clipboard_paste_honors_the_inner_terminals_bracketed_paste_mode() {
        assert_eq!(
            terminal_paste_payload(b"hello\nworld", false).unwrap(),
            b"hello\nworld"
        );
        assert_eq!(
            terminal_paste_payload(b"hello\nworld", true).unwrap(),
            b"\x1b[200~hello\nworld\x1b[201~"
        );
        assert!(terminal_paste_payload(b"unsafe\x1b[201~suffix", true).is_err());
    }

    #[test]
    fn copied_terminal_text_trims_padding_and_excludes_chrome() {
        let pane = PaneId::new("host-a", "work", "w6:p1");
        let mut parser = vt100::Parser::new(2, 10, 0);
        parser.process(b"one   \r\ntwo");
        let mut selection = TerminalSelection {
            pane,
            anchor: CellPosition { row: 0, column: 0 },
            head: CellPosition { row: 1, column: 9 },
            viewport_offset: 0,
            captured_rows: BTreeMap::new(),
            dragging: true,
            forwarded_click: None,
        };
        capture_selection_viewport(&mut selection, parser.screen());

        assert_eq!(selected_terminal_text(&selection), "one\ntwo");
    }

    #[test]
    fn copied_terminal_text_spans_scrollback_and_the_live_viewport() {
        let pane = PaneId::new("host-a", "work", "w6:p1");
        let mut current = vt100::Parser::new(3, 12, 0);
        current.process(b"line3\r\nline4\r\nline5");
        let mut older = vt100::Parser::new(3, 12, 0);
        older.process(b"old1\r\nold2\r\nline3");
        let mut selection = TerminalSelection {
            pane,
            anchor: CellPosition { row: -2, column: 0 },
            head: CellPosition { row: 2, column: 11 },
            viewport_offset: 0,
            captured_rows: BTreeMap::new(),
            dragging: true,
            forwarded_click: None,
        };
        capture_selection_viewport(&mut selection, current.screen());
        selection.viewport_offset = 2;
        capture_selection_viewport(&mut selection, older.screen());

        assert_eq!(
            selected_terminal_text(&selection),
            "old1\nold2\nline3\nline4\nline5"
        );
    }

    #[test]
    fn routed_scroll_frames_extend_selection_and_retain_traversed_rows() {
        let pane = PaneId::new("host-a", "work", "w6:p1");
        let rows = |text: &[u8]| {
            let mut parser = vt100::Parser::new(3, 12, 0);
            parser.process(text);
            capture_screen_rows(parser.screen())
        };
        let current = rows(b"line3\r\nline4\r\nline5");
        let one_up = rows(b"old2\r\nline3\r\nline4");
        let two_up = rows(b"old1\r\nold2\r\nline3");
        assert_eq!(
            viewport_shift_distance(&current, &one_up, SelectionAutoscrollDirection::Up),
            Some(1)
        );

        let mut selection = TerminalSelection {
            pane: pane.clone(),
            anchor: CellPosition { row: 2, column: 11 },
            head: CellPosition { row: 0, column: 0 },
            viewport_offset: 0,
            captured_rows: BTreeMap::new(),
            dragging: true,
            forwarded_click: None,
        };
        super::capture_selection_rows(&mut selection, current.clone());
        let mut app = App {
            selection: Some(selection),
            selection_autoscroll: Some(SelectionAutoscroll {
                pane: pane.clone(),
                direction: SelectionAutoscrollDirection::Up,
                column: 0,
                pending_lines: 1,
            }),
            ..App::default()
        };

        update_selection_after_frame(
            &mut app,
            &pane,
            Some(SelectionAutoscrollDirection::Up),
            Some(&current),
            one_up.clone(),
        );
        assert_eq!(app.selection.as_ref().unwrap().viewport_offset, 1);
        update_selection_after_frame(
            &mut app,
            &pane,
            Some(SelectionAutoscrollDirection::Up),
            Some(&one_up),
            two_up,
        );
        let selection = app.selection.as_ref().unwrap();
        assert_eq!(selection.viewport_offset, 2);
        assert_eq!(
            selected_terminal_text(selection),
            "old1\nold2\nline3\nline4\nline5"
        );
    }

    #[test]
    fn clipboard_confirmation_takes_priority_over_route_status() {
        let app = App {
            message: Some("read-only route".to_owned()),
            clipboard_feedback: Some(ClipboardFeedback {
                text: "copied 42 characters to system clipboard".to_owned(),
                expires_at: Instant::now() + Duration::from_secs(1),
            }),
            ..App::default()
        };

        assert_eq!(
            displayed_message(&app),
            Some("copied 42 characters to system clipboard")
        );
    }

    /// Which pane asks for control is still a frontend decision; whether it
    /// gets control is the daemon's, and is tested against the broker.
    #[test]
    fn only_the_selected_pane_asks_for_control() {
        let selected = PaneId::new("host-a", "dev", "w1:p1");
        let other = PaneId::new("host-a", "dev", "w1:p2");

        assert_eq!(
            desired_access(&selected, &selected),
            TerminalAccess::Control
        );
        assert_eq!(desired_access(&other, &selected), TerminalAccess::Observe);
    }

    #[test]
    fn default_selection_skips_disconnected_targets() {
        let unavailable_key = TargetSession::new("a", "dev");
        let live_key = TargetSession::new("b", "dev");
        let live_pane = PaneId::new("b", "dev", "w1:p1");
        let snapshot = NormalizedSnapshot::from_value(
            &live_key,
            &json!({"panes": [{"pane_id": "w1:p1"}], "focused_pane_id": "w1:p1"}),
        );
        let mut state = FederationState::default();
        state.targets.insert(
            unavailable_key.clone(),
            runtime(
                unavailable_key,
                TargetConnectionState::Backoff { attempt: 1 },
                None,
            ),
        );
        state.targets.insert(
            live_key.clone(),
            runtime(live_key, TargetConnectionState::Live, Some(snapshot)),
        );
        let mut app = App::default();

        reconcile_selection(&state, &mut app);

        assert_eq!(app.selected_pane, Some(live_pane));
    }

    #[test]
    fn startup_selection_moves_to_the_most_active_session() {
        let idle_key = TargetSession::new("host-a", "default");
        let active_key = TargetSession::new("host-a", "work");
        let idle_pane = PaneId::new("host-a", "default", "w1:p1");
        let active_pane = PaneId::new("host-a", "work", "w6:p1");
        let idle_snapshot = NormalizedSnapshot::from_value(
            &idle_key,
            &json!({"panes": [{"pane_id": "w1:p1"}], "focused_pane_id": "w1:p1"}),
        );
        let active_snapshot = NormalizedSnapshot::from_value(
            &active_key,
            &json!({
                "workspaces": [{"workspace_id": "w6"}],
                "panes": [{"pane_id": "w6:p1"}],
                "agents": [{"pane_id": "w6:p1"}],
                "focused_pane_id": "w6:p1"
            }),
        );
        let mut state = FederationState::default();
        state.targets.insert(
            idle_key.clone(),
            runtime(idle_key, TargetConnectionState::Live, Some(idle_snapshot)),
        );
        let mut app = App::default();

        reconcile_selection(&state, &mut app);
        assert_eq!(app.selected_pane, Some(idle_pane.clone()));

        state.targets.insert(
            active_key.clone(),
            runtime(
                active_key,
                TargetConnectionState::Live,
                Some(active_snapshot),
            ),
        );
        reconcile_selection(&state, &mut app);
        assert_eq!(app.selected_pane, Some(active_pane));

        app.selected_pane = Some(idle_pane.clone());
        app.selection_explicit = true;
        reconcile_selection(&state, &mut app);
        assert_eq!(app.selected_pane, Some(idle_pane));
    }

    #[test]
    fn restores_only_an_exact_live_qualified_pane() {
        let key = TargetSession::new("host-a", "work");
        let pane = PaneId::new("host-a", "work", "w6:p1");
        let snapshot = NormalizedSnapshot::from_value(
            &key,
            &json!({"panes": [{"pane_id": "w6:p1"}], "focused_pane_id": "w6:p1"}),
        );
        let mut state = FederationState::default();
        state.targets.insert(
            key.clone(),
            runtime(key, TargetConnectionState::Live, Some(snapshot)),
        );
        let mut app = App {
            restore_pending: Some(pane.clone()),
            ..App::default()
        };

        reconcile_selection(&state, &mut app);

        assert_eq!(app.selected_pane, Some(pane));
        assert!(app.selection_explicit);
        assert!(app.restore_pending.is_none());
    }

    #[test]
    fn retains_restore_intent_while_its_target_is_reconnecting() {
        let key = TargetSession::new("host-a", "work");
        let pane = PaneId::new("host-a", "work", "w6:p1");
        let mut state = FederationState::default();
        state.targets.insert(
            key.clone(),
            runtime(key, TargetConnectionState::Connecting, None),
        );
        let mut app = App {
            restore_pending: Some(pane.clone()),
            ..App::default()
        };

        reconcile_selection(&state, &mut app);

        assert_eq!(app.restore_pending, Some(pane));
        assert!(app.selected_pane.is_none());
    }

    #[test]
    fn strips_control_characters_from_server_labels() {
        assert_eq!(safe_text("safe\u{1b}[31m\nname"), "safe [31m name");
    }

    #[test]
    fn renders_terminal_cells_into_the_main_surface() {
        let mut parser = vt100::Parser::new(3, 10, 0);
        parser.process(b"hello");
        let backend = ratatui::backend::TestBackend::new(10, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_vt_screen(frame, parser.screen(), frame.area(), true, None))
            .unwrap();

        let rendered = (0..5)
            .map(|column| terminal.backend().buffer()[(column, 0)].symbol())
            .collect::<String>();
        assert_eq!(rendered, "hello");
    }

    #[test]
    fn renders_active_drag_selection_as_a_visible_highlight() {
        let pane = PaneId::new("host-a", "work", "w6:p1");
        let mut parser = vt100::Parser::new(1, 10, 0);
        parser.process(b"hello");
        let selection = TerminalSelection {
            pane,
            anchor: CellPosition { row: 0, column: 1 },
            head: CellPosition { row: 0, column: 3 },
            viewport_offset: 0,
            captured_rows: BTreeMap::new(),
            dragging: true,
            forwarded_click: None,
        };
        let backend = ratatui::backend::TestBackend::new(10, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_vt_screen(
                    frame,
                    parser.screen(),
                    frame.area(),
                    false,
                    Some(&selection),
                )
            })
            .unwrap();

        assert!(
            !terminal.backend().buffer()[(0, 0)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        for column in 1..=3 {
            assert!(
                terminal.backend().buffer()[(column, 0)]
                    .modifier
                    .contains(Modifier::REVERSED)
            );
        }
        assert!(
            !terminal.backend().buffer()[(4, 0)]
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn maps_server_split_geometry_into_the_local_terminal_surface() {
        let key = TargetSession::new("host-a", "dev");
        let left = PaneId::new("host-a", "dev", "w1:p1");
        let right = PaneId::new("host-a", "dev", "w1:p2");
        let snapshot = NormalizedSnapshot::from_value(
            &key,
            &json!({
                "panes": [
                    {"pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1"},
                    {"pane_id": "w1:p2", "workspace_id": "w1", "tab_id": "w1:t1"}
                ],
                "layouts": [{
                    "workspace_id": "w1",
                    "tab_id": "w1:t1",
                    "zoomed": false,
                    "area": {"x": 10, "y": 2, "width": 100, "height": 20},
                    "focused_pane_id": "w1:p1",
                    "panes": [
                        {"pane_id": "w1:p1", "focused": true, "rect": {"x": 10, "y": 2, "width": 50, "height": 20}},
                        {"pane_id": "w1:p2", "focused": false, "rect": {"x": 60, "y": 2, "width": 50, "height": 20}}
                    ]
                }]
            }),
        );
        let mut state = FederationState::default();
        state.targets.insert(
            key.clone(),
            runtime(key, TargetConnectionState::Live, Some(snapshot)),
        );

        let panes = visible_pane_areas(
            &state,
            Some(&left),
            ratatui::layout::Rect::new(28, 1, 92, 39),
        );

        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0], (left, ratatui::layout::Rect::new(28, 1, 46, 39)));
        assert_eq!(panes[1], (right, ratatui::layout::Rect::new(74, 1, 46, 39)));

        let app = App {
            selected_pane: Some(panes[0].0.clone()),
            ..App::default()
        };
        assert_eq!(
            clamped_pane_position(
                ratatui::layout::Position::new(0, 0),
                &state,
                &app,
                ratatui::layout::Rect::new(28, 1, 92, 39),
                &panes[0].0,
            ),
            Some(CellPosition { row: 0, column: 0 })
        );
        assert_eq!(
            clamped_pane_position(
                ratatui::layout::Position::new(119, 39),
                &state,
                &app,
                ratatui::layout::Rect::new(28, 1, 92, 39),
                &panes[0].0,
            ),
            Some(CellPosition {
                row: 36,
                column: 41,
            })
        );
    }

    #[test]
    fn prefix_navigation_selects_tabs_and_workspaces_locally() {
        let key = TargetSession::new("host-a", "dev");
        let snapshot = NormalizedSnapshot::from_value(
            &key,
            &json!({
                "workspaces": [
                    {"workspace_id": "w1", "active_tab_id": "w1:t1"},
                    {"workspace_id": "w2", "active_tab_id": "w2:t1"}
                ],
                "tabs": [
                    {"tab_id": "w1:t1", "workspace_id": "w1"},
                    {"tab_id": "w1:t2", "workspace_id": "w1"},
                    {"tab_id": "w2:t1", "workspace_id": "w2"}
                ],
                "panes": [
                    {"pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1"},
                    {"pane_id": "w1:p2", "workspace_id": "w1", "tab_id": "w1:t2"},
                    {"pane_id": "w2:p1", "workspace_id": "w2", "tab_id": "w2:t1"}
                ],
                "layouts": [
                    layout("w1", "w1:t1", "w1:p1"),
                    layout("w1", "w1:t2", "w1:p2"),
                    layout("w2", "w2:t1", "w2:p1")
                ]
            }),
        );
        let mut state = FederationState::default();
        state.targets.insert(
            key.clone(),
            runtime(key, TargetConnectionState::Live, Some(snapshot)),
        );
        let mut app = App {
            selected_pane: Some(PaneId::new("host-a", "dev", "w1:p1")),
            ..App::default()
        };

        assert_eq!(
            sidebar_pane_at_row(&state, app.selected_pane.as_ref(), 0),
            Some(PaneId::new("host-a", "dev", "w1:p1"))
        );
        assert_eq!(
            sidebar_pane_at_row(&state, app.selected_pane.as_ref(), 2),
            Some(PaneId::new("host-a", "dev", "w2:p1"))
        );
        update_sidebar_hit_areas(&state, &mut app, ratatui::layout::Rect::new(0, 0, 28, 10));
        assert_eq!(app.sidebar_hit_areas.len(), 3);
        assert_eq!(
            app.sidebar_hit_areas[2].area,
            ratatui::layout::Rect::new(0, 3, 27, 1)
        );
        assert_eq!(
            app.sidebar_hit_areas[2].pane,
            Some(PaneId::new("host-a", "dev", "w2:p1"))
        );
        assert_eq!(
            tab_at_column(&state, app.selected_pane.as_ref(), 1),
            Some(crate::model::TabId::new("host-a", "dev", "w1:t1"))
        );
        assert_eq!(
            tab_at_column(&state, app.selected_pane.as_ref(), 10),
            Some(crate::model::TabId::new("host-a", "dev", "w1:t2"))
        );

        cycle_tab(&state, &mut app, 1);
        assert_eq!(
            app.selected_pane,
            Some(PaneId::new("host-a", "dev", "w1:p2"))
        );

        select_workspace(&state, &mut app, 1);
        assert_eq!(
            app.selected_pane,
            Some(PaneId::new("host-a", "dev", "w2:p1"))
        );
    }

    #[test]
    fn sidebar_activates_the_original_pressed_item_on_release() {
        let old = PaneId::new("host-a", "dev", "w1:p1");
        let pressed = PaneId::new("host-a", "dev", "w2:p1");
        let mut app = App {
            selected_pane: Some(old),
            sidebar_press: Some(pressed.clone()),
            swallow_left_gesture: true,
            ..App::default()
        };

        finish_ui_left_gesture(&mut app);

        assert_eq!(app.selected_pane, Some(pressed));
        assert!(app.selection_explicit);
        assert!(!app.swallow_left_gesture);
        assert!(app.sidebar_press.is_none());
    }

    #[tokio::test]
    async fn sidebar_scrolls_all_workspaces_and_keeps_mouse_targets_aligned() {
        let state = overflowing_sidebar_state(15);
        let frame_area = ratatui::layout::Rect::new(0, 0, 120, 6);
        let sidebar_area = ui_areas(frame_area).0;
        let mut app = App {
            selected_pane: Some(PaneId::new("host-a", "dev", "w01:p1")),
            last_frame_area: Some(frame_area),
            ..App::default()
        };

        update_sidebar_hit_areas(&state, &mut app, sidebar_area);
        let visible_height = sidebar_content_height(sidebar_area);
        let last_initial_pane = format!("w{:02}:p1", visible_height - 1);
        assert_eq!(app.sidebar_offset, 0);
        assert_eq!(app.sidebar_hit_areas.len(), visible_height);
        assert_eq!(
            app.sidebar_hit_areas
                .last()
                .and_then(|hit| hit.pane.as_ref()),
            Some(&PaneId::new("host-a", "dev", &last_initial_pane))
        );
        let transport = crate::config::TransportConfig::default();

        handle_mouse(
            MouseInput {
                code: 65,
                column: 2,
                row: 2,
                release: false,
            },
            &state,
            &transport,
            &mut app,
        )
        .await
        .unwrap();
        assert_eq!(app.sidebar_offset, 3);
        assert!(!app.sidebar_follow_selected);

        update_sidebar_hit_areas(&state, &mut app, sidebar_area);
        assert_eq!(app.sidebar_offset, 3);
        assert_eq!(
            app.sidebar_hit_areas[0].area,
            ratatui::layout::Rect::new(
                0,
                sidebar_block().inner(sidebar_areas(sidebar_area).0).y,
                27,
                1,
            )
        );
        assert_eq!(
            app.sidebar_hit_areas[0].pane,
            Some(PaneId::new("host-a", "dev", "w03:p1"))
        );

        select_pane(&mut app, PaneId::new("host-a", "dev", "w15:p1"));
        update_sidebar_hit_areas(&state, &mut app, sidebar_area);
        assert_eq!(app.sidebar_offset, 16 - visible_height);
        assert_eq!(
            app.sidebar_hit_areas
                .last()
                .and_then(|hit| hit.pane.as_ref()),
            Some(&PaneId::new("host-a", "dev", "w15:p1"))
        );

        let shorter_sidebar = ratatui::layout::Rect::new(0, 0, 28, 3);
        update_sidebar_hit_areas(&state, &mut app, shorter_sidebar);
        assert_eq!(
            app.sidebar_offset,
            16 - sidebar_content_height(shorter_sidebar)
        );
        assert_eq!(
            app.sidebar_hit_areas
                .last()
                .and_then(|hit| hit.pane.as_ref()),
            Some(&PaneId::new("host-a", "dev", "w15:p1"))
        );
    }

    fn layout(workspace: &str, tab: &str, pane: &str) -> serde_json::Value {
        json!({
            "workspace_id": workspace,
            "tab_id": tab,
            "zoomed": false,
            "area": {"x": 0, "y": 0, "width": 80, "height": 24},
            "focused_pane_id": pane,
            "panes": [{
                "pane_id": pane,
                "focused": true,
                "rect": {"x": 0, "y": 0, "width": 80, "height": 24}
            }]
        })
    }

    fn overflowing_sidebar_state(workspace_count: usize) -> FederationState {
        let key = TargetSession::new("host-a", "dev");
        let mut workspaces = Vec::new();
        let mut tabs = Vec::new();
        let mut panes = Vec::new();
        let mut layouts = Vec::new();
        for index in 1..=workspace_count {
            let workspace = format!("w{index:02}");
            let tab = format!("{workspace}:t1");
            let pane = format!("{workspace}:p1");
            workspaces.push(json!({
                "workspace_id": workspace,
                "active_tab_id": tab,
            }));
            tabs.push(json!({
                "tab_id": tab,
                "workspace_id": workspace,
            }));
            panes.push(json!({
                "pane_id": pane,
                "workspace_id": workspace,
                "tab_id": tab,
            }));
            layouts.push(layout(&workspace, &tab, &pane));
        }
        let snapshot = NormalizedSnapshot::from_value(
            &key,
            &json!({
                "focused_pane_id": "w01:p1",
                "workspaces": workspaces,
                "tabs": tabs,
                "panes": panes,
                "layouts": layouts,
            }),
        );
        let mut state = FederationState::default();
        state.targets.insert(
            key.clone(),
            runtime(key, TargetConnectionState::Live, Some(snapshot)),
        );
        state
    }

    fn layout_with_panes(
        workspace: &str,
        tab: &str,
        focused: &str,
        panes: &[(&str, u16, u16, u16, u16)],
    ) -> serde_json::Value {
        json!({
            "workspace_id": workspace,
            "tab_id": tab,
            "zoomed": false,
            "area": {"x": 0, "y": 0, "width": 80, "height": 20},
            "focused_pane_id": focused,
            "panes": panes.iter().map(|(pane, x, y, width, height)| json!({
                "pane_id": pane,
                "focused": *pane == focused,
                "rect": {"x": x, "y": y, "width": width, "height": height}
            })).collect::<Vec<_>>()
        })
    }

    fn runtime(
        key: TargetSession,
        connection: TargetConnectionState,
        snapshot: Option<NormalizedSnapshot>,
    ) -> TargetRuntimeState {
        TargetRuntimeState {
            key,
            endpoint: "test".to_owned(),
            connection,
            update_mode: crate::state::TargetUpdateMode::Polling,
            event_error: None,
            connection_generation: 1,
            selected_herdr_bin: Some("herdr".to_owned()),
            snapshot: snapshot.map(Arc::new),
            last_error: None,
            last_success: None,
            retry_at: None,
        }
    }
}
