use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use crate::config::{Target, TransportConfig};
use crate::transport::build_ssh_command;

const CLIPBOARD_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardDelivery {
    Native,
    Osc52Requested,
}

#[derive(Debug)]
pub struct UploadedFile {
    pub path: String,
    pub bytes: usize,
    pub mime: &'static str,
    /// What the host computed over the file it stored.
    ///
    /// Carried out rather than checked here, because only the caller holds the
    /// other half: the digest the sender attested to. Comparing the two is what
    /// verifies a transfer end to end, and it is the only comparison that
    /// survives a transfer assembled from more than one attempt.
    pub digest: String,
}

/// Where a transfer's bytes are going.
#[derive(Debug, Clone, Copy)]
pub enum Staging<'a> {
    /// A private directory of its own, under a name the caller chose or the
    /// flavor's own.
    Fresh { name: Option<&'a str> },
    /// One that already holds the beginning of this transfer.
    Resume { path: &'a str, staged: u64 },
}

/// One attempt at moving a transfer to a host.
#[derive(Debug, Clone, Copy)]
pub struct TransferPlan<'a> {
    pub media: ClipboardMedia,
    pub staging: Staging<'a>,
    /// The whole content's length, including anything already staged.
    pub length: u64,
}

impl TransferPlan<'_> {
    /// How much of the content is already on the host.
    pub fn staged(&self) -> u64 {
        match self.staging {
            Staging::Fresh { .. } => 0,
            Staging::Resume { staged, .. } => staged,
        }
    }

    /// How much this attempt is responsible for.
    fn remaining(&self) -> u64 {
        match self.staging {
            Staging::Fresh { .. } => self.length,
            Staging::Resume { staged, .. } => self.length.saturating_sub(staged),
        }
    }
}

/// What became of one attempt.
#[derive(Debug)]
pub enum Transferred {
    Complete(UploadedFile),
    /// The source stopped before the declared length, and what did arrive is
    /// still on the host.
    ///
    /// Not an error, because the difference between an interruption and a
    /// refusal is not visible from here: a stream that stops short is a dropped
    /// connection or a withdrawal depending on facts only the caller has. So
    /// the bytes are left where they are and named, and whoever knows which one
    /// happened decides whether to keep or discard them.
    Interrupted {
        path: String,
        staged: u64,
    },
}

impl ClipboardDelivery {
    pub fn feedback(self, characters: usize) -> String {
        match self {
            Self::Native => format!("copied {characters} characters to system clipboard"),
            Self::Osc52Requested => {
                format!("requested terminal clipboard copy of {characters} characters (OSC 52)")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardContext {
    Desktop,
    Ssh,
    NestedHerdr,
}

impl ClipboardContext {
    fn label(self) -> &'static str {
        match self {
            Self::Desktop => "local desktop",
            Self::Ssh => "remote process over SSH",
            Self::NestedHerdr => "process nested inside Herdr",
        }
    }
}

#[derive(Debug, Clone)]
struct CommandSpec {
    program: &'static str,
    arguments: Vec<String>,
}

impl CommandSpec {
    fn new(program: &'static str, arguments: &[&str]) -> Self {
        Self {
            program,
            arguments: arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
        }
    }
}

/// A byte pattern a format guarantees at a fixed offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Marker {
    offset: usize,
    bytes: &'static [u8],
}

impl Marker {
    fn matches(self, payload: &[u8]) -> bool {
        payload
            .get(self.offset..self.offset.saturating_add(self.bytes.len()))
            .is_some_and(|window| window == self.bytes)
    }
}

/// A clipboard flavor Super-Herdr knows how to move.
///
/// The transfer path is deliberately type-agnostic: it moves bytes, verifies a
/// byte count and digest, and injects a path. Everything format-specific lives
/// in this descriptor, so widening the bridge to another flavor is a table
/// entry rather than a second code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardMedia {
    /// MIME type as the desktop clipboard names it.
    pub mime: &'static str,
    /// Extension given to the uploaded file, when the flavor has one. It
    /// always comes from this table and never from the clipboard, so no
    /// untrusted text reaches the remote command. `None` means the payload is
    /// written with no extension at all.
    pub extension: Option<&'static str>,
    /// Alternatives that identify the format, any one of which is enough. Each
    /// alternative is a set of markers that must all match, because a format
    /// like WebP is only identified by two patterns at different offsets, and
    /// one like GIF has more than one valid form. Empty means the format has no
    /// signature worth checking.
    signatures: &'static [&'static [Marker]],
}

pub const PNG: ClipboardMedia = ClipboardMedia {
    mime: "image/png",
    extension: Some("png"),
    signatures: &[&[Marker {
        offset: 0,
        bytes: b"\x89PNG\r\n\x1a\n",
    }]],
};

pub const JPEG: ClipboardMedia = ClipboardMedia {
    mime: "image/jpeg",
    extension: Some("jpg"),
    signatures: &[&[Marker {
        offset: 0,
        bytes: b"\xff\xd8\xff",
    }]],
};

/// RIFF alone also begins AVI and WAV, so the container tag at offset 8 is what
/// actually identifies WebP.
pub const WEBP: ClipboardMedia = ClipboardMedia {
    mime: "image/webp",
    extension: Some("webp"),
    signatures: &[&[
        Marker {
            offset: 0,
            bytes: b"RIFF",
        },
        Marker {
            offset: 8,
            bytes: b"WEBP",
        },
    ]],
};

pub const GIF: ClipboardMedia = ClipboardMedia {
    mime: "image/gif",
    extension: Some("gif"),
    signatures: &[
        &[Marker {
            offset: 0,
            bytes: b"GIF87a",
        }],
        &[Marker {
            offset: 0,
            bytes: b"GIF89a",
        }],
    ],
};

/// Little-endian and big-endian TIFF differ in their header.
pub const TIFF: ClipboardMedia = ClipboardMedia {
    mime: "image/tiff",
    extension: Some("tif"),
    signatures: &[
        &[Marker {
            offset: 0,
            bytes: b"II*\x00",
        }],
        &[Marker {
            offset: 0,
            bytes: b"MM\x00*",
        }],
    ],
};

pub const PDF: ClipboardMedia = ClipboardMedia {
    mime: "application/pdf",
    extension: Some("pdf"),
    signatures: &[&[Marker {
        offset: 0,
        bytes: b"%PDF-",
    }]],
};

/// SVG is XML text, which may open with a declaration, a comment, a byte order
/// mark, or the root element. There is no prefix worth trusting, so it carries
/// on the digest alone like any unrecognized flavor.
pub const SVG: ClipboardMedia = ClipboardMedia {
    mime: "image/svg+xml",
    extension: Some("svg"),
    signatures: &[],
};

impl ClipboardMedia {
    /// Reject bytes that cannot be what the clipboard claimed.
    ///
    /// A format without a signature still uploads: the byte count and digest
    /// are what prove the transfer, and refusing an unrecognized payload would
    /// only block flavors this table has not learned yet.
    fn validate(self, bytes: &[u8]) -> Result<()> {
        if self.signatures.is_empty() {
            return Ok(());
        }
        let recognized = self
            .signatures
            .iter()
            .any(|alternative| alternative.iter().all(|marker| marker.matches(bytes)));
        if !recognized {
            bail!("clipboard does not contain {} data", self.mime);
        }
        Ok(())
    }

    /// The suffix a macOS pasteboard read gives its temporary file.
    ///
    /// Only that path still needs one: a staged transfer takes its name from
    /// the caller or from [`payload_name`](Self::payload_name), inside a
    /// private directory of its own.
    #[cfg(target_os = "macos")]
    fn file_suffix(self) -> Result<String> {
        Ok(match self.payload_name()?.split_once('.') {
            Some((_, extension)) => format!(".{extension}"),
            None => String::new(),
        })
    }

    /// What a payload of this flavor is called when nobody named it.
    ///
    /// The extension comes from this table rather than from the wire, and stays
    /// inert regardless: the resulting path is pasted into a pane, where a name
    /// that is not plain text would be a command somebody's shell runs.
    fn payload_name(self) -> Result<String> {
        let Some(extension) = self.extension else {
            return Ok("payload".to_owned());
        };
        if extension.is_empty() || !extension.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("unsupported clipboard media extension");
        }
        Ok(format!("payload.{extension}"))
    }
}

/// A payload whose type this bridge does not recognize.
///
/// It carries no extension of its own: a clipboard flavor with no entry in the
/// table is written as a bare `payload` unless the caller named it. The byte
/// count and digest are what prove the transfer either way.
pub const OPAQUE: ClipboardMedia = ClipboardMedia {
    mime: "application/octet-stream",
    extension: None,
    signatures: &[],
};

/// The longest name a caller may ask for.
///
/// Well under any filesystem's limit, because the point is not to fit but to
/// stay something a person can read in a refusal and in a path pasted into a
/// pane.
const MAX_TRANSFER_NAME_BYTES: usize = 240;

/// What a transfer is called on the target host.
///
/// This once could not be asked for at all: the name was interpolated into a
/// remote shell script, where anything from the wire would have been
/// attacker-influenced text in a command, guarded only by a sanitizer that has
/// to stay correct forever. Two things changed, and both are load-bearing. The
/// name now travels to the staging script as data on its standard input rather
/// than as part of the script, so it is never parsed as shell in the first
/// place; and the script refuses a separator itself, so neither side is trusted
/// alone.
///
/// The resulting verified path is shell-quoted by clients before it is pasted,
/// so ordinary names may contain spaces, punctuation, and Unicode without
/// becoming terminal syntax. What remains forbidden here is what cannot be one
/// path component or cannot survive the line framing used to hand the name to
/// the staging script. A name that does not qualify is refused rather than
/// mangled, because silently renaming a file tells the caller it got something
/// different.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedName(String);

impl StagedName {
    /// Resolve what a caller asked for, falling back to the flavor's own name.
    pub fn resolve(requested: Option<&str>, media: ClipboardMedia) -> Result<Self> {
        let Some(requested) = requested else {
            return Ok(Self(media.payload_name()?));
        };
        if requested.is_empty() || requested.len() > MAX_TRANSFER_NAME_BYTES {
            bail!(
                "a transfer name must be between 1 and {MAX_TRANSFER_NAME_BYTES} bytes; \
                 this one is {}",
                requested.len()
            );
        }
        if requested == "." || requested == ".." || requested.contains("..") {
            bail!("a transfer cannot be named {requested:?}");
        }
        if requested.contains('/') || requested.chars().any(char::is_control) {
            bail!("a transfer name must be one printable path component");
        }
        Ok(Self(requested.to_owned()))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

/// Flavors the bridge can move, in the order they are preferred when a
/// clipboard offers several at once. PNG stays first so that the common case,
/// a screenshot, keeps behaving exactly as it always has.
pub const KNOWN_MEDIA: &[ClipboardMedia] = &[PNG, JPEG, WEBP, GIF, TIFF, PDF, SVG];

/// Resolve a media type a caller names to a flavor this bridge can carry.
///
/// An unrecognized type is carried opaquely rather than refused: the transfer
/// is proven by its byte count and digest, not by its name, and refusing here
/// would block every type the table has not learned.
pub fn media_for_mime(mime: &str) -> ClipboardMedia {
    KNOWN_MEDIA
        .iter()
        .copied()
        .find(|media| media.mime == mime)
        .unwrap_or(OPAQUE)
}

/// A clipboard type list is metadata, not payload, but it is still written by
/// whichever program owns the clipboard, so it is bounded before it is read.
const MAXIMUM_TYPE_LIST_BYTES: usize = 64 * 1024;
/// Chunk size for a streamed upload. Large enough to keep SSH busy, small
/// enough that nothing buffers a whole payload.
const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const MAXIMUM_REPORTED_TYPES: usize = 12;
const MAXIMUM_TYPE_NAME_CHARS: usize = 80;

/// How many times the pasteboard has been written to, if the platform counts.
///
/// macOS keeps a change count that moves on every copy whatever was copied, so
/// comparing it across a wait answers the question the file probe cannot:
/// whether a copy happened at all. Without it, "no file was found" and "you
/// never copied anything" are the same sentence.
#[cfg(target_os = "macos")]
async fn clipboard_generation() -> Option<u64> {
    let spec = CommandSpec::new(
        "osascript",
        &[
            "-e",
            "use framework \"Foundation\"",
            "-e",
            "return (current application's NSPasteboard's generalPasteboard()'s changeCount()) as text",
        ],
    );
    read_reference_output(&spec).await.ok()?.trim().parse().ok()
}

/// Nothing else counts copies, so nothing else can answer the question.
#[cfg(not(target_os = "macos"))]
async fn clipboard_generation() -> Option<u64> {
    None
}

/// How often a waiting check looks again, and the longest it will be asked to.
const PROBE_INTERVAL: Duration = Duration::from_millis(400);
pub const MAXIMUM_PROBE_WAIT: Duration = Duration::from_secs(300);

/// Watch the clipboard until it names a file, or until the wait runs out.
///
/// Copying a file and then running a command is only one order; the other is
/// running the command and then copying, and it is the order that works when
/// getting the command into the terminal is what overwrote the clipboard.
async fn await_offered_files(wait: Duration) -> FileProbe {
    let deadline = Instant::now() + wait;
    loop {
        let probe = probe_offered_files().await;
        if !probe.paths.is_empty() || Instant::now() >= deadline {
            return probe;
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

pub async fn diagnostic_lines(wait: Option<Duration>) -> Vec<String> {
    let context = clipboard_context();
    let writer = native_writer();
    let reader = native_reader();
    let (copy_method, copy_acknowledgement) = if prefers_terminal_copy() {
        ("OSC 52 terminal request".to_owned(), "not acknowledged")
    } else if let Some(writer) = writer {
        (format!("native ({})", writer.program), "acknowledged")
    } else {
        (
            "OSC 52 fallback (no native writer found)".to_owned(),
            "not acknowledged",
        )
    };
    let paste_method = if context == ClipboardContext::Desktop {
        reader.map_or_else(
            || "unavailable (no native reader found)".to_owned(),
            |reader| format!("native ({})", reader.program),
        )
    } else {
        "unavailable to an SSH/nested process; paste through the local terminal or run Super-Herdr on the desktop".to_owned()
    };
    let media_method = if context == ClipboardContext::Desktop {
        media_reader_label(PNG)
    } else {
        "unavailable to an SSH/nested process".to_owned()
    };
    let offered = if context == ClipboardContext::Desktop {
        let names = offered_type_names().await;
        if names.is_empty() {
            "not reported by this desktop".to_owned()
        } else {
            names.join(", ")
        }
    } else {
        "unavailable to an SSH/nested process".to_owned()
    };

    // Reported by name because the path is the diagnosis: a file copied in a
    // file manager is a reference rather than bytes, and whether this can
    // follow it is the difference between a copy that works and one that
    // silently does nothing.
    // Waiting is only ever worth it on a desktop; nothing will arrive on a
    // clipboard this process cannot reach however long it watches.
    let waiting = wait.filter(|_| context == ClipboardContext::Desktop);
    let before = match waiting {
        Some(_) => clipboard_generation().await,
        None => None,
    };
    let probe = match waiting {
        Some(wait) => await_offered_files(wait).await,
        None => probe_offered_files().await,
    };
    let copies = match before {
        Some(before) => clipboard_generation()
            .await
            .map(|after| after.saturating_sub(before)),
        None => None,
    };
    let referenced = match probe.paths.as_slice() {
        [] if context == ClipboardContext::Desktop => {
            "none (copy a file in a file manager, then run this with --wait 20)".to_owned()
        }
        [] => "unavailable to an SSH/nested process".to_owned(),
        paths => paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
    };

    let mut lines = vec![
        format!("context: {}", context.label()),
        format!("copy: {copy_method} ({copy_acknowledgement})"),
        format!("paste action: {paste_method}"),
        format!("media paste action: {media_method}"),
        format!("clipboard offers: {offered}"),
        format!("copied file: {referenced}"),
    ];
    // Only when nothing was found, because that is the only time the reader's
    // own account tells anybody anything they cannot already see.
    if probe.paths.is_empty() && context == ClipboardContext::Desktop {
        lines.push(match probe.attempts.as_slice() {
            [] => "file readers: none available on this desktop".to_owned(),
            attempts => format!("file readers: {}", attempts.join("; ")),
        });
        // The two failures that look identical from here are "you copied a file
        // and this could not read it" and "you never copied anything". A
        // platform that counts copies can tell them apart outright.
        if let Some(copies) = copies {
            lines.push(match copies {
                0 => "nothing was copied while this waited: the clipboard was never written to, so what it holds is whatever was there before".to_owned(),
                1 => "one copy happened while this waited, and it was not a file".to_owned(),
                copies => format!("{copies} copies happened while this waited, and the last was not a file"),
            });
        }
    }
    lines.extend([
        format!(
            "uploadable flavors: {}",
            KNOWN_MEDIA
                .iter()
                .map(|media| media.mime)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        "clipboard payloads are neither inspected by this check nor written to logs".to_owned(),
    ]);
    lines
}

pub fn write_text(text: &str) -> Result<ClipboardDelivery> {
    if !prefers_terminal_copy()
        && native_writer().is_some_and(|spec| run_writer(&spec, text.as_bytes()))
    {
        return Ok(ClipboardDelivery::Native);
    }
    write_osc52(text)?;
    Ok(ClipboardDelivery::Osc52Requested)
}

pub async fn read_text(maximum_bytes: usize) -> Result<String> {
    if clipboard_context() != ClipboardContext::Desktop {
        bail!(
            "local clipboard reads are unavailable over SSH or inside Herdr; paste through the local terminal or run Super-Herdr on the desktop"
        );
    }
    let Some(spec) = native_reader() else {
        bail!("no native clipboard reader is available");
    };
    let bytes = read_command_bytes(spec, maximum_bytes, "system clipboard").await?;
    String::from_utf8(bytes).context("system clipboard does not contain UTF-8 text")
}

/// Ask the clipboard which flavors it currently holds.
///
/// Assuming a flavor is how the bridge used to fail: a JPEG on the clipboard
/// produced "does not contain image/png data", which is true and useless. An
/// empty result means enumeration is unavailable, not that the clipboard is
/// empty, so callers must fall back rather than conclude anything from it.
pub async fn offered_type_names() -> Vec<String> {
    if clipboard_context() != ClipboardContext::Desktop {
        return Vec::new();
    }
    #[cfg(target_os = "linux")]
    {
        let Some(spec) = native_type_lister() else {
            return Vec::new();
        };
        let Ok(bytes) = read_command_bytes(spec, MAXIMUM_TYPE_LIST_BYTES, "clipboard types").await
        else {
            return Vec::new();
        };
        let Ok(text) = String::from_utf8(bytes) else {
            return Vec::new();
        };
        collect_type_names(text.lines())
    }
    #[cfg(target_os = "macos")]
    {
        let spec = CommandSpec::new("osascript", &["-e", "clipboard info"]);
        let Ok(bytes) = read_command_bytes(spec, MAXIMUM_TYPE_LIST_BYTES, "clipboard types").await
        else {
            return Vec::new();
        };
        let Ok(text) = String::from_utf8(bytes) else {
            return Vec::new();
        };
        collect_type_names(macos_type_fields(&text).iter().map(String::as_str))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Vec::new()
    }
}

/// Flavor names are untrusted text from whichever program owns the clipboard.
/// They may be shown, but only stripped of control characters and bounded.
fn collect_type_names<'a>(raw: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut names = Vec::new();
    for candidate in raw {
        let cleaned: String = candidate
            .chars()
            .filter(|character| character.is_ascii_graphic() || *character == ' ')
            .take(MAXIMUM_TYPE_NAME_CHARS)
            .collect();
        let cleaned = cleaned.trim().to_owned();
        if cleaned.is_empty() || names.contains(&cleaned) {
            continue;
        }
        names.push(cleaned);
        if names.len() >= MAXIMUM_REPORTED_TYPES {
            break;
        }
    }
    names
}

/// The flavors the bridge can actually move, in table preference order.
pub async fn offered_media() -> Vec<ClipboardMedia> {
    let names = offered_type_names().await;
    KNOWN_MEDIA
        .iter()
        .copied()
        .filter(|media| names.iter().any(|name| name == media.mime))
        .collect()
}

/// Read whichever supported flavor the clipboard is offering.
/// The largest name a file reference may carry before it is refused.
const MAXIMUM_FILE_REFERENCE_BYTES: usize = 8 * 1024;

/// How much of a failed reader's complaint is read, and how much is shown.
const MAXIMUM_COMPLAINT_BYTES: u64 = 4 * 1024;
const MAXIMUM_COMPLAINT_CHARS: usize = 200;

/// The files a clipboard is pointing at, rather than any it contains.
///
/// Copying a file in a file manager does not put its bytes on a clipboard. It
/// puts a reference — `public.file-url` on macOS, `text/uri-list` under
/// Wayland and X11 — so a bridge that only ever looks for data flavours sees a
/// clipboard it does not recognise and gives up, which is exactly what happened
/// to every file anybody tried to copy. Following the reference is the whole of
/// the fix: the bytes are on the local disk, where this process can read them.
///
/// A selection is copied as a selection, so all of it is returned. Each file
/// carries its own name, each is verified on its own, and a failure belongs to
/// the file it happened to — there is nothing about the second file that the
/// first has not already answered.
pub async fn offered_files() -> Vec<PathBuf> {
    probe_offered_files().await.paths
}

/// The files a clipboard names, and what each reader said on the way there.
///
/// The account exists because "no file was copied" and "a file was copied and
/// this could not read it" produced the same empty answer, which is the one
/// question a person staring at `copied file: none` actually needs answered.
struct FileProbe {
    paths: Vec<PathBuf>,
    attempts: Vec<String>,
}

async fn probe_offered_files() -> FileProbe {
    if clipboard_context() != ClipboardContext::Desktop {
        return FileProbe {
            paths: Vec::new(),
            attempts: Vec::new(),
        };
    }
    probe_readers(file_reference_readers()).await
}

/// Ask each reader in turn. Split from the clipboard context it runs in so the
/// order it tries things, and the account it keeps, can be tested with readers
/// that behave like the real ones without needing a desktop to run on.
async fn probe_readers(readers: Vec<(&'static str, CommandSpec)>) -> FileProbe {
    let mut attempts = Vec::new();
    // A reader that fails hands over to the next one; a reader that *answers*
    // ends the walk, empty-handed or not. "Nothing is on the clipboard" is an
    // answer, and asking the fallback to disagree with it is how a clipboard
    // holding plain text ends up being described as holding a file.
    for (label, spec) in readers {
        let Ok(text) = read_reference_output(&spec).await.inspect_err(|complaint| {
            attempts.push(format!("{label}: {complaint}"));
        }) else {
            continue;
        };
        let named = local_file_references(&text);
        // Named and present are different claims. `the clipboard as «class
        // furl»` will coerce arbitrary copied text into a file reference, so a
        // path that is not on this disk is the reader inventing one rather than
        // reporting one — and a file that is not there cannot be copied anyway.
        let paths: Vec<PathBuf> = named
            .into_iter()
            .filter(|path| fs::metadata(path).is_ok())
            .collect();
        attempts.push(match paths.len() {
            0 => format!("{label}: answered, naming no file on this disk"),
            count => format!("{label}: named {count} file(s)"),
        });
        return FileProbe { paths, attempts };
    }
    FileProbe {
        paths: Vec::new(),
        attempts,
    }
}

/// Run one file-reference reader, answering with its output or with why there
/// was none.
///
/// Separate from [`read_command_bytes`] for one reason: that helper ignores
/// exit status, so a reader that failed outright was indistinguishable from a
/// clipboard holding no file. Here a failure is a failure, and it says so.
async fn read_reference_output(spec: &CommandSpec) -> Result<String, String> {
    let mut child = tokio::process::Command::new(spec.program)
        .args(&spec.arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", spec.program))?;
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err("started without pipes to read".to_owned());
    };
    let mut output = Vec::new();
    let mut complaint = Vec::new();
    let limit = u64::try_from(MAXIMUM_FILE_REFERENCE_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    // Both pipes are drained together; draining one and then the other would
    // deadlock against a reader that fills the pipe it is not being read from.
    let mut bounded_output = (&mut stdout).take(limit);
    let mut bounded_complaint = (&mut stderr).take(MAXIMUM_COMPLAINT_BYTES);
    let drain = async {
        let _ = tokio::join!(
            bounded_output.read_to_end(&mut output),
            bounded_complaint.read_to_end(&mut complaint),
        );
        child.wait().await
    };
    let status = match timeout(CLIPBOARD_COMMAND_TIMEOUT, drain).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => return Err(format!("could not be waited on: {error}")),
        Err(_) => return Err("timed out".to_owned()),
    };
    if !status.success() {
        return Err(reader_complaint(&complaint, status.code()));
    }
    if output.len() > MAXIMUM_FILE_REFERENCE_BYTES {
        return Err(format!(
            "answered more than the {MAXIMUM_FILE_REFERENCE_BYTES}-byte limit"
        ));
    }
    String::from_utf8(output).map_err(|_| "answered bytes that are not text".to_owned())
}

/// A failed reader's own words, bounded and stripped to printable ASCII.
///
/// This is the reader complaining about the clipboard, not the clipboard's
/// contents, but it is still untrusted text on its way to a terminal, so it is
/// treated exactly like the flavor names are.
fn reader_complaint(stderr: &[u8], code: Option<i32>) -> String {
    let text: String = String::from_utf8_lossy(stderr)
        .chars()
        .map(|character| {
            if character.is_ascii_graphic() || character == ' ' {
                character
            } else {
                ' '
            }
        })
        .collect();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let ending = code.map_or_else(
        || "killed by a signal".to_owned(),
        |code| format!("exit {code}"),
    );
    if text.is_empty() {
        return format!("failed ({ending})");
    }
    let text: String = text.chars().take(MAXIMUM_COMPLAINT_CHARS).collect();
    format!("failed ({ending}): {text}")
}

#[cfg(target_os = "linux")]
fn file_reference_readers() -> Vec<(&'static str, CommandSpec)> {
    let mut readers = Vec::new();
    if env::var_os("WAYLAND_DISPLAY").is_some() && command_available("wl-paste") {
        readers.push((
            "wl-paste",
            CommandSpec::new("wl-paste", &["--type", "text/uri-list"]),
        ));
    }
    if env::var_os("DISPLAY").is_some() && command_available("xclip") {
        readers.push((
            "xclip",
            CommandSpec::new(
                "xclip",
                &["-selection", "clipboard", "-t", "text/uri-list", "-o"],
            ),
        ));
    }
    readers
}

/// macOS answers with paths directly rather than URLs, which avoids having to
/// undo percent-encoding on a value that becomes a filesystem path. Each file
/// in the selection comes back on its own line.
///
/// The pasteboard is asked through Foundation first because the AppleScript
/// coercion behind the fallback yields a *single* file however many were
/// copied — it is a coercion to one file reference, not to a selection — and
/// silently pasting one of the four files somebody copied is worse than the
/// error it looks like nothing went wrong. `readObjectsForClasses:` returns the
/// whole selection. Non-file URLs are dropped here as well as in the parser,
/// since a copied link is a link and reading it is not this program's business.
#[cfg(target_os = "macos")]
fn file_reference_readers() -> Vec<(&'static str, CommandSpec)> {
    let foundation = CommandSpec::new(
        "osascript",
        &[
            "-e",
            "use framework \"Foundation\"",
            "-e",
            "use scripting additions",
            "-e",
            "set out to \"\"",
            "-e",
            "set board to current application's NSPasteboard's generalPasteboard()",
            "-e",
            "set refs to board's readObjectsForClasses:{current application's NSURL} options:(missing value)",
            "-e",
            "if refs is not missing value then",
            "-e",
            "repeat with i from 1 to (count of refs)",
            "-e",
            "set one to item i of refs",
            "-e",
            "if (one's isFileURL()) as boolean then set out to out & ((one's |path|()) as text) & linefeed",
            "-e",
            "end repeat",
            "-e",
            "end if",
            "-e",
            "return out",
        ],
    );
    let coercion = CommandSpec::new(
        "osascript",
        &[
            "-e",
            "set out to \"\"",
            "-e",
            "repeat with item_ref in (the clipboard as \u{ab}class furl\u{bb})",
            "-e",
            "set out to out & POSIX path of item_ref & linefeed",
            "-e",
            "end repeat",
            "-e",
            "return out",
        ],
    );
    vec![
        ("osascript (NSPasteboard)", foundation),
        ("osascript (furl coercion)", coercion),
    ]
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn file_reference_readers() -> Vec<(&'static str, CommandSpec)> {
    Vec::new()
}

/// Every local file a clipboard's reference names, in the order given.
///
/// Handles both shapes because both are real: `file://` URIs from a
/// `text/uri-list`, and bare POSIX paths from `osascript`. Anything that is not
/// a local path is skipped rather than guessed at — a `https://` entry is a
/// link somebody copied, not a file this can read — and skipping rather than
/// refusing the lot means one odd entry in a selection does not cost the files
/// beside it.
fn local_file_references(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        // A uri-list may carry comments, and an empty line is not an error.
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            if let Some(rest) = line.strip_prefix("file://") {
                // `file:///path` has an empty authority; anything else names
                // another machine and is not a file on this disk.
                let path = rest.strip_prefix('/').map(|path| format!("/{path}"))?;
                return Some(PathBuf::from(percent_decoded(&path)));
            }
            line.starts_with('/').then(|| PathBuf::from(line))
        })
        .collect()
}

/// Undo the percent-encoding a `file://` URI carries. Left alone on anything
/// malformed, since a path that fails to open reports better than one silently
/// altered.
fn percent_decoded(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(value) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| path.to_owned())
}

pub async fn read_offered_media(maximum_bytes: usize) -> Result<(ClipboardMedia, Vec<u8>)> {
    let names = offered_type_names().await;
    if let Some(media) = KNOWN_MEDIA
        .iter()
        .copied()
        .find(|media| names.iter().any(|name| name == media.mime))
    {
        let bytes = read_media(media, maximum_bytes).await?;
        return Ok((media, bytes));
    }
    if names.is_empty() {
        // Enumeration is unavailable here, so ask for the historical flavor
        // rather than refusing a clipboard that may well hold one.
        let bytes = read_media(PNG, maximum_bytes).await?;
        return Ok((PNG, bytes));
    }
    bail!(
        "clipboard offers {}, none of which can be uploaded yet",
        names.join(", ")
    );
}

pub async fn read_media(media: ClipboardMedia, maximum_bytes: usize) -> Result<Vec<u8>> {
    if clipboard_context() != ClipboardContext::Desktop {
        bail!(
            "local clipboard media reads are unavailable over SSH or inside Herdr; run Super-Herdr on the desktop"
        );
    }
    #[cfg(target_os = "linux")]
    {
        let Some(spec) = native_media_reader(media) else {
            bail!("no native {} clipboard reader is available", media.mime);
        };
        let description = format!("clipboard {}", media.mime);
        let bytes = read_command_bytes(spec, maximum_bytes, &description).await?;
        media.validate(&bytes)?;
        Ok(bytes)
    }
    #[cfg(target_os = "macos")]
    {
        read_macos_media(media, maximum_bytes).await
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (media, maximum_bytes);
        bail!("clipboard media reads are unsupported on this platform")
    }
}

pub async fn upload_media(
    target: &Target,
    transport: &TransportConfig,
    media: ClipboardMedia,
    bytes: &[u8],
) -> Result<UploadedFile> {
    media.validate(bytes)?;
    // A clipboard flavor names itself; nobody is holding a file whose name this
    // could be.
    let staged = StagedName::resolve(None, media)?;
    let expected_digest = sha256_hex(bytes);
    if let Some(destination) = target.ssh.as_deref() {
        let upload = timeout(
            Duration::from_secs(transport.command_timeout_seconds),
            upload_remote_media(destination, transport, &staged, bytes),
        )
        .await
        .context("clipboard media upload timed out")??;
        if upload.bytes != bytes.len() || upload.digest != expected_digest {
            // A refusal that leaves the payload behind is not a refusal: the
            // host keeps a file nothing verified, and nothing will collect it.
            remove_remote_upload(destination, transport, &upload.path).await;
            bail!("remote clipboard media verification failed");
        }
        return Ok(UploadedFile {
            path: upload.path,
            bytes: upload.bytes,
            mime: media.mime,
            digest: upload.digest,
        });
    }
    upload_local_media(media, &staged, bytes, &expected_digest)
}

/// Upload a payload that is being read rather than held.
///
/// The clipboard path has its bytes in memory and computes a digest before
/// sending. A device file does not, and hashing it in a separate earlier pass
/// would attest to what the source held during that pass rather than to what
/// was sent. Hashing on the way past attests to exactly the bytes that went
/// out, in one pass, with no window in between.
///
/// `expected_bytes` is enforced rather than believed: a source that yields more
/// is cut off rather than allowed to write unbounded data onto the host, and
/// one that yields fewer is refused as truncated. Each refusal names the check
/// that failed, because only a short read is likely to be worth retrying, and
/// every refusal removes whatever reached the host.
///
/// No overall timeout applies. A large transfer legitimately outlives the
/// per-command timeout that bounds clipboard-sized uploads; progress is bounded
/// by the source and by `expected_bytes` instead.
pub async fn upload_stream<R>(
    target: &Target,
    transport: &TransportConfig,
    plan: TransferPlan<'_>,
    source: R,
) -> Result<Transferred>
where
    R: AsyncRead + Unpin,
{
    // Judged before a connection is opened, because a name is the one thing
    // about a transfer that can be decided without moving anything.
    let staged = match plan.staging {
        Staging::Fresh { name } => Some(StagedName::resolve(name, plan.media)?),
        Staging::Resume { path, .. } => {
            anyhow::ensure!(
                removable_upload_directory(path).is_some(),
                "a transfer cannot be resumed into a path this bridge did not stage"
            );
            None
        }
    };
    let receipt = if let Some(destination) = target.ssh.as_deref() {
        remote_attempt(destination, transport, staged.as_ref(), plan, source).await?
    } else {
        local_attempt(staged.as_ref(), plan, source).await?
    };
    finish_attempt(target, transport, plan, receipt).await
}

/// Judge what the host now holds against what the whole transfer declared.
///
/// The digest is not compared here. The host computed one over the file it
/// stored, and the only thing worth comparing it against is what the sender
/// attested to — which arrives after the last byte, and belongs to whoever is
/// holding the sender's end.
async fn finish_attempt(
    target: &Target,
    transport: &TransportConfig,
    plan: TransferPlan<'_>,
    receipt: RemoteUploadReceipt,
) -> Result<Transferred> {
    let total = receipt.bytes as u64;
    if total < plan.length {
        return Ok(Transferred::Interrupted {
            path: receipt.path,
            staged: total,
        });
    }
    if total > plan.length {
        // Unreachable from a relay that stops at the declared length, and not
        // something anyone should resume from, so it goes rather than lingers.
        discard_upload(target, transport, &receipt.path).await;
        bail!(
            "host stored {total} bytes of a transfer that declared {}",
            plan.length
        );
    }
    Ok(Transferred::Complete(UploadedFile {
        path: receipt.path,
        bytes: receipt.bytes,
        mime: plan.media.mime,
        digest: receipt.digest,
    }))
}

/// Read one file back off a target host.
///
/// The digest is computed by the host in its own pass and sent ahead of the
/// bytes, which is a deliberate difference from the upload direction and worth
/// stating. An uploading client hashes while it sends, so its digest attests to
/// the bytes that actually went out. A portable shell cannot do that — there is
/// no way to tee a stream through a hash without leaving POSIX behind — so this
/// hashes first and sends second. The consequence is precise: a file modified
/// between those two passes fails verification at the client. That is a false
/// refusal rather than a false acceptance, which is the direction an
/// unavoidable weakness has to point.
///
/// The path arrives on standard input rather than in the script, for the same
/// reason a transfer's name does: nothing from the wire is ever parsed as
/// shell. It is refused unless it names a readable regular file, so a directory
/// or a device does not become a stream with no end.
const DOWNLOAD_SCRIPT: &str = r#"set -eu
IFS= read -r source_file
[ -f "$source_file" ] || exit 1
[ -r "$source_file" ] || exit 1
source_size=$(wc -c < "$source_file" | tr -d '[:space:]')
source_digest=$(sha256sum "$source_file" | awk '{print $1}')
printf '%s\t%s\n' "$source_size" "$source_digest"
cat "$source_file"
"#;

/// A file on a target, open and ready to be read.
///
/// What a client is told about it comes from the host: how long it is, and what
/// it hashes to. The daemon counts what passes through and computes nothing,
/// exactly as it does in the other direction.
pub struct SourceFile {
    /// The last component of the path, so a client has something to call it.
    /// Never a path: what a client does with a name is its own business, and
    /// handing it separators would make that business worse.
    pub name: String,
    pub length: u64,
    pub digest: String,
    reader: Box<dyn AsyncRead + Send + Unpin>,
    /// Held so that dropping this kills the reader rather than leaving it
    /// filling a pipe nobody is emptying.
    _child: Option<tokio::process::Child>,
}

impl AsyncRead for SourceFile {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.reader).poll_read(context, buffer)
    }
}

/// Open a file on a target for reading.
///
/// A client names the path, which is the reverse of the upload direction, where
/// it never does. That is not a widening of what a client may reach: it holds
/// the pane's control lease, so it can already type `cat` into a shell on that
/// host and read the same bytes back through the terminal. This is the same
/// authority through a channel that can verify what it moved.
pub async fn open_source(
    target: &Target,
    transport: &TransportConfig,
    path: &str,
) -> Result<SourceFile> {
    anyhow::ensure!(
        !path.is_empty() && !path.contains('\n') && !path.chars().any(char::is_control),
        "a source path must be one line of ordinary text"
    );
    let name = path
        .rsplit_once('/')
        .map_or(path, |(_, tail)| tail)
        .to_owned();
    anyhow::ensure!(!name.is_empty(), "a source path must name a file");

    let Some(destination) = target.ssh.as_deref() else {
        return open_local_source(path, name).await;
    };
    let mut command = build_ssh_command(destination, transport, DOWNLOAD_SCRIPT.to_owned());
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start the SSH file read")?;
    let mut input = child
        .stdin
        .take()
        .context("the SSH file read did not expose stdin")?;
    let stdout = child
        .stdout
        .take()
        .context("the SSH file read did not expose stdout")?;
    input
        .write_all(format!("{path}\n").as_bytes())
        .await
        .context("failed to send the source path")?;
    input.shutdown().await.ok();
    drop(input);

    let mut reader = tokio::io::BufReader::new(stdout);
    let mut header = String::new();
    let read = timeout(
        Duration::from_secs(transport.command_timeout_seconds),
        reader.read_line(&mut header),
    )
    .await
    .context("the host took too long to describe the file")?
    .context("failed to read what the host said about the file")?;
    if read == 0 {
        bail!("the host has no readable file at that path");
    }
    let (length, digest) = parse_source_header(&header)?;
    Ok(SourceFile {
        name,
        length,
        digest,
        reader: Box::new(reader),
        _child: Some(child),
    })
}

/// The same, when the target is this machine.
async fn open_local_source(path: &str, name: String) -> Result<SourceFile> {
    let metadata = tokio::fs::metadata(path)
        .await
        .context("there is no readable file at that path")?;
    anyhow::ensure!(metadata.is_file(), "that path does not name a regular file");
    // Read once for the digest and once for the bytes, which is what the
    // remote side does, so both directions fail the same way on a file that
    // changes underneath them.
    let contents = tokio::fs::read(path)
        .await
        .context("failed to read the file")?;
    let digest = sha256_hex(&contents);
    let file = tokio::fs::File::open(path)
        .await
        .context("failed to open the file")?;
    Ok(SourceFile {
        name,
        length: metadata.len(),
        digest,
        reader: Box::new(file),
        _child: None,
    })
}

fn parse_source_header(line: &str) -> Result<(u64, String)> {
    let (length, digest) = line
        .trim_end_matches('\n')
        .split_once('\t')
        .context("the host described the file in a shape this does not understand")?;
    let length: u64 = length
        .trim()
        .parse()
        .context("the host reported a length that is not a number")?;
    let digest = digest.trim();
    anyhow::ensure!(
        digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()),
        "the host reported a digest that is not a SHA-256"
    );
    Ok((length, digest.to_owned()))
}

/// How much of an interrupted transfer the host still holds.
///
/// The answer comes from the file rather than from anything remembered, because
/// an attempt that died mid-chunk left a length nobody predicted. A path that
/// is not one this bridge staged, or a file that is no longer there, is not an
/// error worth distinguishing: both mean there is nothing to resume, and the
/// transfer starts again from nothing.
pub async fn staged_bytes(target: &Target, transport: &TransportConfig, path: &str) -> Option<u64> {
    removable_upload_directory(path)?;
    let Some(destination) = target.ssh.as_deref() else {
        return fs::metadata(path).ok().map(|file| file.len());
    };
    let mut command = build_ssh_command(destination, transport, STAGED_SIZE_SCRIPT.to_owned());
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut input = child.stdin.take()?;
    let _ = input.write_all(format!("{path}\n").as_bytes()).await;
    let _ = input.shutdown().await;
    drop(input);
    let output = timeout(
        Duration::from_secs(transport.command_timeout_seconds),
        child.wait_with_output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

/// Remove a staged upload the caller has decided not to accept.
///
/// [`upload_stream`] verifies what it sent against what the host stored, but a
/// relay has a second promise to check that only it can see: the digest its own
/// sender attested to, which arrives after the last byte. A transfer that fails
/// that check has already reached the host, and leaving it there would leave a
/// verified-looking artifact reachable by another route — a path injected into
/// a pane cannot be told apart from one that passed.
///
/// Nothing here is taken from the wire. The path came from a receipt this
/// process read, and is checked against the shape the staging script produces
/// before anything is removed.
pub async fn discard_upload(target: &Target, transport: &TransportConfig, path: &str) {
    if let Some(destination) = target.ssh.as_deref() {
        remove_remote_upload(destination, transport, path).await;
        return;
    }
    discard_local_upload(Path::new(path));
}

/// Remove a locally staged transfer, directory and all.
///
/// The directory is the unit because that is what was created for it: the file
/// inside carries a caller's name, and removing only the file would leave an
/// empty private directory per refused transfer. The same shape check the
/// remote side applies is applied here, since a path this process assembled is
/// no reason to skip the check that keeps `rm -rf` pointed at one place.
pub(crate) fn discard_local_upload(path: &Path) {
    let Some(directory) = path.to_str().and_then(removable_upload_directory) else {
        return;
    };
    let _ = fs::remove_dir_all(directory);
}

/// One attempt at moving bytes to a host over SSH.
///
/// A fresh transfer and a resumed one differ only in which script runs and what
/// its first line says: a name for a directory to be made, or the path of one
/// that already holds the beginning of this file. Everything after that — the
/// copy, the receipt, and what a failure leaves behind — is the same, which is
/// the point of doing it this way rather than writing a second transfer path.
async fn remote_attempt<R>(
    destination: &str,
    transport: &TransportConfig,
    staged: Option<&StagedName>,
    plan: TransferPlan<'_>,
    mut source: R,
) -> Result<RemoteUploadReceipt>
where
    R: AsyncRead + Unpin,
{
    let script = match plan.staging {
        Staging::Fresh { .. } => UPLOAD_SCRIPT,
        Staging::Resume { .. } => RESUME_SCRIPT,
    };
    let mut command = build_ssh_command(destination, transport, script.to_owned());
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start the SSH media upload")?;
    let mut input = child
        .stdin
        .take()
        .context("the SSH media upload did not expose stdin")?;
    let output = child
        .stdout
        .take()
        .context("the SSH media upload did not expose stdout")?;
    // Ahead of the payload, on its own line: framing around the file rather
    // than part of it, which is what keeps it out of the script's own text.
    let heading = match plan.staging {
        Staging::Fresh { .. } => staged
            .context("a fresh transfer needs a name")?
            .as_str()
            .to_owned(),
        Staging::Resume { path, .. } => path.to_owned(),
    };
    input
        .write_all(format!("{heading}\n").as_bytes())
        .await
        .context("failed to send the transfer heading")?;

    let expected = plan.remaining();
    let mut written = 0u64;
    let mut buffer = vec![0u8; STREAM_CHUNK_BYTES];
    // A failure moving the bytes is recorded rather than returned. By the time
    // one can happen the host has already staged part of a file, and the only
    // way to learn where is to finish the exchange and read the receipt.
    let mut failure: Option<anyhow::Error> = None;
    while written < expected {
        let wanted = usize::try_from(expected - written)
            .unwrap_or(STREAM_CHUNK_BYTES)
            .min(STREAM_CHUNK_BYTES);
        match source.read(&mut buffer[..wanted]).await {
            Ok(0) => break,
            Ok(read) => {
                if let Err(error) = input.write_all(&buffer[..read]).await {
                    failure =
                        Some(anyhow::Error::new(error).context("failed to send upload bytes"));
                    break;
                }
                written += read as u64;
            }
            Err(error) => {
                failure =
                    Some(anyhow::Error::new(error).context("failed to read the upload source"));
                break;
            }
        }
    }
    if let Err(error) = input.shutdown().await {
        failure.get_or_insert_with(|| {
            anyhow::Error::new(error).context("failed to finish the media upload")
        });
    }
    drop(input);

    let receipt = read_upload_receipt(output).await;
    let waited = child.wait().await;
    // A receipt is what makes an interruption resumable rather than lost, so a
    // failure with one is reported through the receipt and judged like any
    // other short attempt. A failure without one leaves nothing anybody can
    // name, and there is nothing to do but say so.
    match (failure, receipt) {
        (Some(_), Ok(receipt)) => Ok(receipt),
        (Some(error), Err(_)) => Err(error),
        (None, receipt) => {
            let status = waited.context("failed to wait for the SSH media upload")?;
            if !status.success() {
                bail!("SSH media upload failed (diagnostics redacted)");
            }
            receipt
        }
    }
}

/// The same attempt, when the target is this machine.
///
/// Written to look like its remote sibling on purpose: a private directory with
/// the file inside it, appended to when resuming, and a receipt computed from
/// what is actually on disk rather than from what was believed to be written.
async fn local_attempt<R>(
    staged: Option<&StagedName>,
    plan: TransferPlan<'_>,
    mut source: R,
) -> Result<RemoteUploadReceipt>
where
    R: AsyncRead + Unpin,
{
    let path = match plan.staging {
        Staging::Fresh { .. } => local_staging(staged.context("a fresh transfer needs a name")?)?,
        Staging::Resume { path, .. } => PathBuf::from(path),
    };
    let expected = plan.remaining();
    let mut written = 0u64;
    let mut failure: Option<anyhow::Error> = None;
    {
        let mut sink = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .context("failed to open the local media file")?;
        let mut buffer = vec![0u8; STREAM_CHUNK_BYTES];
        while written < expected {
            let wanted = usize::try_from(expected - written)
                .unwrap_or(STREAM_CHUNK_BYTES)
                .min(STREAM_CHUNK_BYTES);
            match source.read(&mut buffer[..wanted]).await {
                Ok(0) => break,
                Ok(read) => {
                    if let Err(error) = sink.write_all(&buffer[..read]) {
                        failure = Some(
                            anyhow::Error::new(error).context("failed to write the local media"),
                        );
                        break;
                    }
                    written += read as u64;
                }
                Err(error) => {
                    failure =
                        Some(anyhow::Error::new(error).context("failed to read the upload source"));
                    break;
                }
            }
        }
        if let Err(error) = sink.flush() {
            failure.get_or_insert_with(|| {
                anyhow::Error::new(error).context("failed to flush the local media file")
            });
        }
    }
    // Read back rather than counted: the receipt has to describe the file, not
    // the intention, exactly as the remote script's does.
    let stored = fs::read(&path).context("failed to verify the local media file")?;
    if let Some(error) = failure
        && stored.is_empty()
    {
        discard_local_upload(&path);
        return Err(error);
    }
    Ok(RemoteUploadReceipt {
        path: path.display().to_string(),
        bytes: stored.len(),
        digest: sha256_hex(&stored),
    })
}

async fn read_command_bytes(
    spec: CommandSpec,
    maximum_bytes: usize,
    description: &str,
) -> Result<Vec<u8>> {
    let mut child = tokio::process::Command::new(spec.program)
        .args(&spec.arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to start {description} reader {}", spec.program))?;
    let stdout = child
        .stdout
        .take()
        .context("clipboard reader did not expose stdout")?;
    let mut bytes = Vec::new();
    let limit = u64::try_from(maximum_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let read = timeout(
        CLIPBOARD_COMMAND_TIMEOUT,
        stdout.take(limit).read_to_end(&mut bytes),
    )
    .await;
    match read {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            let _ = child.kill().await;
            return Err(error).with_context(|| format!("failed to read {description}"));
        }
        Err(_) => {
            let _ = child.kill().await;
            bail!("{description} read timed out");
        }
    }
    if bytes.len() > maximum_bytes {
        let _ = child.kill().await;
        bail!("{description} exceeds the {maximum_bytes}-byte limit");
    }
    let status = timeout(CLIPBOARD_COMMAND_TIMEOUT, child.wait())
        .await
        .with_context(|| format!("{description} reader timed out"))?
        .with_context(|| format!("failed to wait for the {description} reader"))?;
    if !status.success() {
        bail!("{description} reader exited unsuccessfully");
    }
    Ok(bytes)
}

fn clipboard_context() -> ClipboardContext {
    if env::var_os("HERDR_ENV").as_deref() == Some(OsStr::new("1")) {
        ClipboardContext::NestedHerdr
    } else if env::var_os("SSH_CONNECTION").is_some() || env::var_os("SSH_TTY").is_some() {
        ClipboardContext::Ssh
    } else {
        ClipboardContext::Desktop
    }
}

fn prefers_terminal_copy() -> bool {
    clipboard_context() != ClipboardContext::Desktop
}

fn native_writer() -> Option<CommandSpec> {
    #[cfg(target_os = "macos")]
    if command_available("pbcopy") {
        return Some(CommandSpec::new("pbcopy", &[]));
    }
    #[cfg(target_os = "linux")]
    {
        if env::var_os("WAYLAND_DISPLAY").is_some() && command_available("wl-copy") {
            return Some(CommandSpec::new(
                "wl-copy",
                &["--type", "text/plain;charset=utf-8"],
            ));
        }
        if env::var_os("DISPLAY").is_some() && command_available("xclip") {
            return Some(CommandSpec::new(
                "xclip",
                &["-selection", "clipboard", "-in"],
            ));
        }
        if env::var_os("DISPLAY").is_some() && command_available("xsel") {
            return Some(CommandSpec::new("xsel", &["--clipboard", "--input"]));
        }
    }
    None
}

fn native_reader() -> Option<CommandSpec> {
    #[cfg(target_os = "macos")]
    if command_available("pbpaste") {
        return Some(CommandSpec::new("pbpaste", &[]));
    }
    #[cfg(target_os = "linux")]
    {
        if env::var_os("WAYLAND_DISPLAY").is_some() && command_available("wl-paste") {
            return Some(CommandSpec::new("wl-paste", &["--no-newline"]));
        }
        if env::var_os("DISPLAY").is_some() && command_available("xclip") {
            return Some(CommandSpec::new(
                "xclip",
                &["-selection", "clipboard", "-out"],
            ));
        }
        if env::var_os("DISPLAY").is_some() && command_available("xsel") {
            return Some(CommandSpec::new("xsel", &["--clipboard", "--output"]));
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn native_type_lister() -> Option<CommandSpec> {
    if env::var_os("WAYLAND_DISPLAY").is_some() && command_available("wl-paste") {
        return Some(CommandSpec::new("wl-paste", &["--list-types"]));
    }
    if env::var_os("DISPLAY").is_some() && command_available("xclip") {
        return Some(CommandSpec::new(
            "xclip",
            &["-selection", "clipboard", "-t", "TARGETS", "-o"],
        ));
    }
    None
}

#[cfg(target_os = "linux")]
fn native_media_reader(media: ClipboardMedia) -> Option<CommandSpec> {
    if env::var_os("WAYLAND_DISPLAY").is_some() && command_available("wl-paste") {
        return Some(CommandSpec::new("wl-paste", &["--type", media.mime]));
    }
    if env::var_os("DISPLAY").is_some() && command_available("xclip") {
        return Some(CommandSpec::new(
            "xclip",
            &["-selection", "clipboard", "-type", media.mime, "-out"],
        ));
    }
    None
}

fn media_reader_label(media: ClipboardMedia) -> String {
    #[cfg(target_os = "linux")]
    {
        native_media_reader(media).map_or_else(
            || "unavailable (install wl-clipboard or xclip)".to_owned(),
            |reader| format!("native {} ({})", media.mime, reader.program),
        )
    }
    #[cfg(target_os = "macos")]
    {
        if macos_pasteboard_class(media).is_none() {
            format!("unsupported on macOS ({})", media.mime)
        } else if command_available("osascript") {
            format!("native {} (macOS clipboard via osascript)", media.mime)
        } else {
            "unavailable (osascript not found)".to_owned()
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = media;
        "unsupported on this platform".to_owned()
    }
}

/// macOS names pasteboard flavors by four-character code rather than by MIME,
/// so the two are mapped in both directions from one table.
///
/// An unmapped flavor reports as unsupported instead of guessing a code, which
/// would ask the pasteboard for something that cannot exist.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MACOS_CLASSES: &[(&str, &str)] = &[
    ("image/png", "PNGf"),
    ("image/jpeg", "JPEG"),
    ("image/gif", "GIFf"),
    ("image/tiff", "TIFF"),
    ("application/pdf", "PDF "),
    ("text/rtf", "RTF "),
];

// WebP and SVG have no classic pasteboard class, so they report as unsupported
// on macOS rather than being requested under a code that cannot exist.

/// Flavors that carry no bytes this can upload but are worth naming anyway.
///
/// `furl` is the one that matters. It is what a file manager puts on the
/// pasteboard, so seeing it beside `copied file: none` says the reference was
/// there and this build failed to follow it — a different fault, and a
/// different fix, from nothing having been copied at all.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MACOS_REPORTED_CLASSES: &[(&str, &str)] = &[("file reference", "furl")];

#[cfg(target_os = "macos")]
fn macos_pasteboard_class(media: ClipboardMedia) -> Option<&'static str> {
    MACOS_CLASSES
        .iter()
        .find(|(mime, _)| *mime == media.mime)
        .map(|(_, class)| *class)
}

/// Turn one `clipboard info` report into flavor names.
///
/// The report is a comma-separated list of alternating flavor and size, where a
/// flavor is either `«class XXXX»` or a human name. A class this build does not
/// map is still reported, as `class:XXXX`, so the operator can see what is
/// actually on the clipboard.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn macos_type_fields(report: &str) -> Vec<String> {
    report
        .split(',')
        .map(str::trim)
        .filter_map(|field| {
            let class = field.strip_prefix("\u{ab}class ")?.strip_suffix('\u{bb}')?;
            let class = class.trim_end();
            Some(
                MACOS_CLASSES
                    .iter()
                    .chain(MACOS_REPORTED_CLASSES)
                    .find(|(_, code)| code.trim_end() == class)
                    .map_or_else(|| format!("class:{class}"), |(name, _)| (*name).to_owned()),
            )
        })
        .collect()
}

#[cfg(target_os = "macos")]
async fn read_macos_media(media: ClipboardMedia, maximum_bytes: usize) -> Result<Vec<u8>> {
    let Some(class) = macos_pasteboard_class(media) else {
        bail!("{} cannot be read from the macOS clipboard", media.mime);
    };
    let output = tempfile::Builder::new()
        .prefix("super-herdr-clipboard-")
        .suffix(&media.file_suffix()?)
        .tempfile()
        .context("failed to create a temporary clipboard media file")?;
    let path = output.path().to_owned();
    let script = format!(
        r#"
set outputPath to system attribute "SUPER_HERDR_CLIPBOARD_MEDIA"
set mediaData to the clipboard as «class {class}»
set outputFile to open for access POSIX file outputPath with write permission
try
    set eof outputFile to 0
    write mediaData to outputFile
on error messageText number messageNumber
    close access outputFile
    error messageText number messageNumber
end try
close access outputFile
"#
    );
    let status = timeout(
        CLIPBOARD_COMMAND_TIMEOUT,
        tokio::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .env("SUPER_HERDR_CLIPBOARD_MEDIA", &path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .context("macOS clipboard media read timed out")?
    .context("failed to run the macOS clipboard media reader")?;
    if !status.success() {
        bail!("macOS clipboard does not contain {} data", media.mime);
    }
    let metadata = fs::metadata(&path).context("failed to inspect the clipboard media")?;
    if metadata.len() > u64::try_from(maximum_bytes).unwrap_or(u64::MAX) {
        bail!("clipboard media exceeds the {maximum_bytes}-byte limit");
    }
    let bytes = fs::read(path).context("failed to read the clipboard media")?;
    media.validate(&bytes)?;
    Ok(bytes)
}

struct RemoteUploadReceipt {
    path: String,
    bytes: usize,
    digest: String,
}

/// The remote side of every upload: stage into a private directory, then report
/// what was actually stored.
///
/// The file name is the only part that is not a literal, and it comes from the
/// media table with an alphanumeric-only extension, so the command carries no
/// caller-supplied text.
/// The staging script, which no longer knows what it is about to be called.
///
/// The name arrives as the first line of the same stream that carries the
/// payload, so it is data to this script rather than part of it. That is what
/// makes a caller-supplied name possible at all: nothing from the wire is ever
/// parsed as shell. The separator check here is not redundant with the one the
/// daemon performs — it is the half that holds if the daemon's ever widens.
///
/// The remote login shell runs this, and zsh ties several lowercase names to
/// its own variables — `path` is tied to PATH, so assigning it replaces the
/// command search path with one file and everything after it fails to be found.
/// That is the default shell on macOS, so the names here are chosen to collide
/// with nothing.
/// The same staging, for a transfer that already has a beginning on this host.
///
/// It takes a path rather than a name, and appends rather than creates. The
/// path was produced by the script above and handed back by this host, but it
/// is still checked here for the shape only that script produces: a path is the
/// one thing in this exchange that has made a round trip through somewhere
/// else, and `>>` on a path nobody checked is how a transfer becomes a way to
/// append to an arbitrary file.
///
/// The receipt describes the whole file rather than this attempt, because what
/// matters at the end is the file, and a transfer made of several attempts has
/// no other way to be verified as one thing.
const RESUME_SCRIPT: &str = r#"set -eu
umask 077
IFS= read -r staged_file
case "$staged_file" in
  */super-herdr-clipboard.*/*) ;;
  *) exit 1 ;;
esac
case "$staged_file" in
  *..*) exit 1 ;;
esac
[ -f "$staged_file" ] || exit 1
cat >> "$staged_file"
staged_size=$(wc -c < "$staged_file" | tr -d '[:space:]')
staged_digest=$(sha256sum "$staged_file" | awk '{print $1}')
printf '%s\t%s\t%s\n' "$staged_file" "$staged_size" "$staged_digest"
"#;

/// How much of a transfer a host already holds.
///
/// Asked of the host rather than remembered here: what a resuming sender needs
/// is the offset the next byte belongs at, and only the file knows that. A
/// daemon that restarted, or one whose last attempt died mid-chunk, would
/// otherwise resume from a number that was never true.
const STAGED_SIZE_SCRIPT: &str = r#"set -eu
IFS= read -r staged_file
case "$staged_file" in
  */super-herdr-clipboard.*/*) ;;
  *) exit 1 ;;
esac
case "$staged_file" in
  *..*) exit 1 ;;
esac
[ -f "$staged_file" ] || exit 1
wc -c < "$staged_file" | tr -d '[:space:]'
"#;

const UPLOAD_SCRIPT: &str = r#"set -eu
umask 077
IFS= read -r transfer_name
case "$transfer_name" in
  ""|.|..) exit 1 ;;
  */*) exit 1 ;;
esac
staging_base=${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}
staging_dir=$(mktemp -d "$staging_base/super-herdr-clipboard.XXXXXXXX")
staged_file="$staging_dir/$transfer_name"
cat > "$staged_file"
staged_size=$(wc -c < "$staged_file" | tr -d '[:space:]')
staged_digest=$(sha256sum "$staged_file" | awk '{print $1}')
printf '%s\t%s\t%s\n' "$staged_file" "$staged_size" "$staged_digest"
"#;

async fn read_upload_receipt(output: tokio::process::ChildStdout) -> Result<RemoteUploadReceipt> {
    let mut receipt = Vec::new();
    output
        .take(4097)
        .read_to_end(&mut receipt)
        .await
        .context("failed to read the media upload receipt")?;
    if receipt.len() > 4096 {
        bail!("the media upload returned an oversized receipt");
    }
    parse_remote_upload_receipt(&receipt)
}

async fn upload_remote_media(
    destination: &str,
    transport: &TransportConfig,
    staged: &StagedName,
    bytes: &[u8],
) -> Result<RemoteUploadReceipt> {
    let mut command = build_ssh_command(destination, transport, UPLOAD_SCRIPT.to_owned());
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start SSH clipboard media upload")?;
    let mut input = child
        .stdin
        .take()
        .context("SSH clipboard media upload did not expose stdin")?;
    let output = child
        .stdout
        .take()
        .context("SSH clipboard media upload did not expose stdout")?;
    write_transfer_name(&mut input, staged).await?;
    input
        .write_all(bytes)
        .await
        .context("failed to upload clipboard media bytes")?;
    input
        .shutdown()
        .await
        .context("failed to finish clipboard media upload")?;
    drop(input);
    let receipt = read_upload_receipt(output).await;
    let status = child
        .wait()
        .await
        .context("failed to wait for SSH clipboard media upload")?;
    if !status.success() {
        bail!("SSH clipboard media upload failed (diagnostics redacted)");
    }
    receipt
}

/// Hand the staging script its name, ahead of the payload it belongs to.
///
/// The newline is the whole framing: the script reads one line and everything
/// after it is the file. [`StagedName`] rejects control characters so a name
/// cannot terminate or otherwise confuse that heading.
async fn write_transfer_name<W>(input: &mut W, staged: &StagedName) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    input
        .write_all(format!("{}\n", staged.as_str()).as_bytes())
        .await
        .context("failed to send the transfer name")
}

/// Stage a local transfer the way the remote script does: a private directory,
/// with the file inside it under its own name.
///
/// The shape is deliberately identical to the remote one, so a staged file has
/// one set of rules, one cleanup, and one thing `removable_upload_directory`
/// has to recognize — whichever side it is on.
fn local_staging(staged: &StagedName) -> Result<PathBuf> {
    let directory = tempfile::Builder::new()
        .prefix("super-herdr-clipboard.")
        .tempdir()
        .context("failed to create a local staging directory")?;
    Ok(directory.keep().join(staged.as_str()))
}

/// The directory a receipt's payload lives in, if it is one this bridge made.
///
/// The path comes back from the remote host, so it is treated as untrusted:
/// only a directory this bridge created is ever named for removal.
fn removable_upload_directory(path: &str) -> Option<&str> {
    let directory = path.rsplit_once('/').map(|(head, _)| head)?;
    let name = directory
        .rsplit_once('/')
        .map_or(directory, |(_, tail)| tail);
    if !directory.starts_with('/')
        || !name.starts_with("super-herdr-clipboard.")
        || directory.chars().any(char::is_control)
    {
        return None;
    }
    Some(directory)
}

/// Best-effort removal of an upload that failed verification.
///
/// The directory is sent on stdin rather than interpolated into the command, so
/// the removal carries no remote-supplied text in its arguments, and the remote
/// side re-checks the shape before deleting anything.
async fn remove_remote_upload(destination: &str, transport: &TransportConfig, path: &str) {
    let Some(directory) = removable_upload_directory(path) else {
        return;
    };
    const SCRIPT: &str = r#"set -eu
IFS= read -r staging_dir
case "$staging_dir" in
  /*/super-herdr-clipboard.*) ;;
  *) exit 1 ;;
esac
rm -rf -- "$staging_dir"
"#;
    let mut command = build_ssh_command(destination, transport, SCRIPT.to_owned());
    let Ok(mut child) = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    else {
        return;
    };
    if let Some(mut input) = child.stdin.take() {
        let _ = input.write_all(format!("{directory}\n").as_bytes()).await;
        let _ = input.shutdown().await;
    }
    let _ = timeout(
        Duration::from_secs(transport.command_timeout_seconds),
        child.wait(),
    )
    .await;
}

fn parse_remote_upload_receipt(bytes: &[u8]) -> Result<RemoteUploadReceipt> {
    let text = std::str::from_utf8(bytes).context("remote upload receipt is not UTF-8")?;
    let mut fields = text.trim_end_matches(['\r', '\n']).split('\t');
    let path = fields.next().unwrap_or_default();
    let size = fields.next().unwrap_or_default();
    let digest = fields.next().unwrap_or_default();
    if fields.next().is_some()
        || !std::path::Path::new(path).is_absolute()
        || path.chars().any(char::is_control)
        || digest.len() != 64
        || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("remote clipboard media upload returned an invalid receipt");
    }
    Ok(RemoteUploadReceipt {
        path: path.to_owned(),
        bytes: size
            .parse()
            .context("remote clipboard media upload returned an invalid size")?,
        digest: digest.to_ascii_lowercase(),
    })
}

fn upload_local_media(
    media: ClipboardMedia,
    staged: &StagedName,
    bytes: &[u8],
    expected_digest: &str,
) -> Result<UploadedFile> {
    let path = local_staging(staged)?;
    let mut file = fs::File::create(&path).context("failed to create a local clipboard media")?;
    file.write_all(bytes)
        .context("failed to write the local clipboard media")?;
    file.flush()
        .context("failed to flush the local clipboard media")?;
    drop(file);
    let verified = fs::read(&path).context("failed to verify the local clipboard media")?;
    if verified.len() != bytes.len() || sha256_hex(&verified) != expected_digest {
        discard_local_upload(&path);
        bail!("local clipboard media verification failed");
    }
    Ok(UploadedFile {
        path: path.display().to_string(),
        bytes: bytes.len(),
        mime: media.mime,
        digest: expected_digest.to_owned(),
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn command_available(program: &str) -> bool {
    env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|directory| executable_file(directory.join(program)))
    })
}

fn executable_file(path: PathBuf) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
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

fn run_writer(spec: &CommandSpec, bytes: &[u8]) -> bool {
    let Ok(mut child) = Command::new(spec.program)
        .args(&spec.arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let write_succeeded = child
        .stdin
        .take()
        .is_some_and(|mut input| input.write_all(bytes).is_ok());
    if !write_succeeded {
        let _ = child.kill();
    }
    child
        .wait()
        .is_ok_and(|status| write_succeeded && status.success())
}

fn write_osc52(text: &str) -> Result<()> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut output = io::stdout().lock();
    output
        .write_all(b"\x1b]52;c;")
        .and_then(|()| output.write_all(encoded.as_bytes()))
        .and_then(|()| output.write_all(b"\x07"))
        .and_then(|()| output.flush())
        .context("failed to write the clipboard bridge sequence")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::{
        ClipboardContext, ClipboardMedia, DOWNLOAD_SCRIPT, GIF, JPEG, KNOWN_MEDIA, OPAQUE, PDF,
        PNG, RESUME_SCRIPT, SVG, StagedName, Staging, TIFF, TransferPlan, Transferred,
        UPLOAD_SCRIPT, UploadedFile, WEBP, discard_local_upload, open_source,
        parse_remote_upload_receipt, removable_upload_directory, sha256_hex, staged_bytes,
        upload_stream,
    };
    use crate::config::{Target, TransportConfig};

    fn context(
        ssh_connection: Option<&OsStr>,
        ssh_tty: Option<&OsStr>,
        herdr_env: Option<&OsStr>,
    ) -> ClipboardContext {
        if herdr_env == Some(OsStr::new("1")) {
            ClipboardContext::NestedHerdr
        } else if ssh_connection.is_some() || ssh_tty.is_some() {
            ClipboardContext::Ssh
        } else {
            ClipboardContext::Desktop
        }
    }

    #[test]
    fn nested_herdr_and_ssh_use_the_outer_terminal_clipboard() {
        assert_eq!(
            context(None, None, Some(OsStr::new("1"))),
            ClipboardContext::NestedHerdr
        );
        assert_eq!(
            context(Some(OsStr::new("peer")), None, None),
            ClipboardContext::Ssh
        );
        assert_eq!(context(None, None, None), ClipboardContext::Desktop);
    }

    #[test]
    fn validates_media_and_remote_size_digest_receipts() {
        let png = b"\x89PNG\r\n\x1a\nbody";
        assert!(PNG.validate(png).is_ok());
        assert!(PNG.validate(b"not png").is_err());
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let receipt = parse_remote_upload_receipt(
            b"/tmp/super-herdr/image.png\t12\tba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\n",
        )
        .unwrap();
        assert_eq!(receipt.path, "/tmp/super-herdr/image.png");
        assert_eq!(receipt.bytes, 12);
        assert!(parse_remote_upload_receipt(b"relative\t12\tbad\n").is_err());
    }

    #[test]
    fn a_format_without_a_signature_is_carried_rather_than_refused() {
        // The byte count and digest prove the transfer. Refusing an unknown
        // payload would block every flavor the table has not learned yet.
        const OPAQUE: ClipboardMedia = ClipboardMedia {
            mime: "application/octet-stream",
            extension: Some("bin"),
            signatures: &[],
        };
        assert!(OPAQUE.validate(b"anything at all").is_ok());
        assert_eq!(OPAQUE.payload_name().unwrap(), "payload.bin");
        // SVG is XML text and is carried the same way.
        assert!(SVG.validate(b"<?xml version=\"1.0\"?><svg/>").is_ok());
        assert!(SVG.validate(b"not xml at all").is_ok());
    }

    #[test]
    fn each_flavor_recognizes_only_its_own_bytes() {
        let samples: &[(ClipboardMedia, &[u8])] = &[
            (PNG, b"\x89PNG\r\n\x1a\nbody"),
            (JPEG, b"\xff\xd8\xff\xe0body"),
            (GIF, b"GIF89abody"),
            (TIFF, b"II*\x00body"),
            (PDF, b"%PDF-1.7body"),
            (WEBP, b"RIFF\x24\x00\x00\x00WEBPVP8 "),
        ];
        for (media, bytes) in samples {
            assert!(
                media.validate(bytes).is_ok(),
                "{} rejected its own bytes",
                media.mime
            );
            for (other, _) in samples {
                if other.mime != media.mime {
                    assert!(
                        other.validate(bytes).is_err(),
                        "{} accepted {} bytes",
                        other.mime,
                        media.mime
                    );
                }
            }
        }
    }

    #[test]
    fn a_signature_is_checked_at_its_own_offset() {
        // RIFF alone is also AVI and WAV; the tag at offset 8 is what makes it
        // WebP, so a prefix-only check would accept the wrong container.
        assert!(WEBP.validate(b"RIFF\x24\x00\x00\x00AVI LIST").is_err());
        assert!(WEBP.validate(b"RIFF").is_err());
        assert!(WEBP.validate(b"RIFF\x24\x00\x00\x00WEBP").is_ok());
    }

    #[test]
    fn a_format_with_several_valid_forms_accepts_each() {
        assert!(GIF.validate(b"GIF87abody").is_ok());
        assert!(GIF.validate(b"GIF89abody").is_ok());
        assert!(GIF.validate(b"GIF88abody").is_err());
        // Little-endian and big-endian TIFF.
        assert!(TIFF.validate(b"II*\x00body").is_ok());
        assert!(TIFF.validate(b"MM\x00*body").is_ok());
        assert!(TIFF.validate(b"MM*\x00body").is_err());
    }

    fn local_target() -> Target {
        Target {
            name: "local".to_owned(),
            ssh: None,
            discover_sessions: false,
            session: None,
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
        }
    }

    /// A plan for a transfer starting from nothing.
    fn fresh(name: Option<&str>, media: ClipboardMedia, length: u64) -> TransferPlan<'_> {
        TransferPlan {
            media,
            staging: Staging::Fresh { name },
            length,
        }
    }

    fn complete(transferred: Transferred) -> UploadedFile {
        match transferred {
            Transferred::Complete(uploaded) => uploaded,
            Transferred::Interrupted { path, staged } => {
                panic!("expected a complete transfer, got {staged} bytes at {path}")
            }
        }
    }

    #[tokio::test]
    async fn a_streamed_upload_is_verified_against_the_bytes_that_were_sent() {
        let payload = vec![7u8; 200_000];
        let uploaded = complete(
            upload_stream(
                &local_target(),
                &TransportConfig::default(),
                fresh(None, OPAQUE, payload.len() as u64),
                payload.as_slice(),
            )
            .await
            .unwrap(),
        );
        let stored = std::fs::read(&uploaded.path).unwrap();
        assert_eq!(stored, payload);
        assert_eq!(uploaded.bytes, payload.len());
        // The digest travels out rather than being checked here, because the
        // other half of the comparison belongs to whoever holds the sender.
        assert_eq!(uploaded.digest, sha256_hex(&payload));
        // An unrecognized type is written with no extension at all.
        assert!(uploaded.path.ends_with("/payload"), "{}", uploaded.path);
        discard_local_upload(std::path::Path::new(&uploaded.path));
    }

    #[tokio::test]
    async fn a_short_source_stops_rather_than_failing_and_keeps_what_arrived() {
        // Not an error: a stream that stops short is a dropped connection or a
        // withdrawal, and which one it was is not visible from here. The bytes
        // stay where they are and the caller decides.
        let payload = vec![1u8; 1024];
        let transferred = upload_stream(
            &local_target(),
            &TransportConfig::default(),
            fresh(None, PNG, 4096),
            payload.as_slice(),
        )
        .await
        .unwrap();
        let Transferred::Interrupted { path, staged } = transferred else {
            panic!("a short source must not look like a finished transfer");
        };
        assert_eq!(staged, 1024);
        assert_eq!(std::fs::read(&path).unwrap(), payload);
        discard_local_upload(std::path::Path::new(&path));
    }

    #[tokio::test]
    async fn a_source_longer_than_declared_is_cut_off_rather_than_believed() {
        let payload = vec![9u8; 10_000];
        let uploaded = complete(
            upload_stream(
                &local_target(),
                &TransportConfig::default(),
                fresh(None, OPAQUE, 4096),
                payload.as_slice(),
            )
            .await
            .unwrap(),
        );
        // Only the declared length reaches the host: a lying length must not
        // write unbounded data there.
        assert_eq!(uploaded.bytes, 4096);
        discard_local_upload(std::path::Path::new(&uploaded.path));
    }

    #[tokio::test]
    async fn a_resumed_transfer_appends_and_verifies_as_one_file() {
        let payload: Vec<u8> = (0..30_000_u32).map(|index| index as u8).collect();
        let (target, transport) = (local_target(), TransportConfig::default());

        // First attempt: the source stops a third of the way through.
        let Transferred::Interrupted { path, staged } = upload_stream(
            &target,
            &transport,
            fresh(Some("archive.tar"), OPAQUE, payload.len() as u64),
            &payload[..10_000],
        )
        .await
        .unwrap() else {
            panic!("the first attempt should not have finished");
        };
        assert_eq!(staged, 10_000);

        // What the host holds is what decides where the next byte goes, and it
        // is asked rather than assumed.
        let offset = staged_bytes(&target, &transport, &path).await.unwrap();
        assert_eq!(offset, 10_000);

        // Second attempt: also short. Resuming is not a promise to finish.
        let Transferred::Interrupted { path, staged } = upload_stream(
            &target,
            &transport,
            TransferPlan {
                media: OPAQUE,
                staging: Staging::Resume {
                    path: &path,
                    staged: offset,
                },
                length: payload.len() as u64,
            },
            &payload[10_000..25_000],
        )
        .await
        .unwrap() else {
            panic!("the second attempt should not have finished either");
        };
        assert_eq!(staged, 25_000);

        // Third attempt finishes it.
        let uploaded = complete(
            upload_stream(
                &target,
                &transport,
                TransferPlan {
                    media: OPAQUE,
                    staging: Staging::Resume {
                        path: &path,
                        staged,
                    },
                    length: payload.len() as u64,
                },
                &payload[25_000..],
            )
            .await
            .unwrap(),
        );

        assert_eq!(uploaded.bytes, payload.len());
        assert_eq!(std::fs::read(&uploaded.path).unwrap(), payload);
        // The digest spans the whole file rather than the last attempt, which
        // is the only thing a transfer assembled from three pieces can be
        // verified against.
        assert_eq!(uploaded.digest, sha256_hex(&payload));
        assert!(uploaded.path.ends_with("/archive.tar"), "{}", uploaded.path);
        discard_local_upload(std::path::Path::new(&uploaded.path));
    }

    #[tokio::test]
    async fn a_transfer_cannot_be_resumed_into_a_path_this_bridge_did_not_stage() {
        // The path is the one value in the exchange that has made a round trip
        // through somewhere else, and appending to it is what a resume does.
        let outside = tempfile::NamedTempFile::new().unwrap();
        let error = upload_stream(
            &local_target(),
            &TransportConfig::default(),
            TransferPlan {
                media: OPAQUE,
                staging: Staging::Resume {
                    path: outside.path().to_str().unwrap(),
                    staged: 0,
                },
                length: 16,
            },
            b"sixteen bytes!!!".as_slice(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("did not stage"), "{error}");
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"");
    }

    #[tokio::test]
    async fn nothing_is_staged_where_there_is_nothing_to_resume() {
        let (target, transport) = (local_target(), TransportConfig::default());
        assert!(
            staged_bytes(&target, &transport, "/etc/passwd")
                .await
                .is_none(),
            "a path this bridge did not stage has no length worth reporting"
        );
        assert!(
            staged_bytes(
                &target,
                &transport,
                "/tmp/super-herdr-clipboard.nonexistent/payload"
            )
            .await
            .is_none()
        );
    }

    #[test]
    fn clipboard_file_references_resolve_to_local_paths_and_nothing_else() {
        use super::local_file_references;
        use std::path::PathBuf;

        let one = |path: &str| vec![PathBuf::from(path)];

        assert_eq!(
            local_file_references("file:///Users/example/Downloads/source.c"),
            one("/Users/example/Downloads/source.c")
        );
        // What osascript answers with: the path itself.
        assert_eq!(
            local_file_references("/Users/example/Downloads/source.c\n"),
            one("/Users/example/Downloads/source.c")
        );
        // Percent-encoding is undone, because a file:// URI carries it and a
        // path with a literal %20 in it is not the file anybody copied.
        assert_eq!(
            local_file_references("file:///home/example/two%20words.c"),
            one("/home/example/two words.c")
        );

        // A selection is copied as a selection. All of it, in order.
        assert_eq!(
            local_file_references(
                "# comment\r\nfile:///home/example/a.c\r\nfile:///home/example/b.c",
            ),
            vec![
                PathBuf::from("/home/example/a.c"),
                PathBuf::from("/home/example/b.c")
            ]
        );

        // One entry that is not a local file does not cost the files beside it.
        assert_eq!(
            local_file_references("https://example.com/thing.c\nfile:///home/example/real.c"),
            one("/home/example/real.c")
        );

        for text in [
            // A link somebody copied is not a file on this disk.
            "https://example.com/thing.c",
            // Another machine's file, which this cannot read.
            "file://otherhost/home/example/a.c",
            "",
            "   ",
            // Relative, so there is no way to know what it is relative to.
            "Downloads/a.c",
        ] {
            assert!(
                local_file_references(text).is_empty(),
                "{text:?} should not resolve to a local file"
            );
        }
    }

    /// A file on the pasteboard has to be visible in the diagnostic, because
    /// `copied file: none` on its own cannot tell an operator whether nothing
    /// was copied or whether the reference was there and went unread.
    #[test]
    fn a_copied_file_is_named_among_the_offered_flavors() {
        use super::macos_type_fields;

        assert_eq!(
            macos_type_fields("\u{ab}class furl\u{bb}, 132, \u{ab}class utf8\u{bb}, 41"),
            vec!["file reference".to_owned(), "class:utf8".to_owned()]
        );
        assert_eq!(
            macos_type_fields("\u{ab}class PNGf\u{bb}, 4096"),
            vec!["image/png".to_owned()]
        );
    }

    /// The bug this guards: a reader that fails outright used to be reported
    /// as a clipboard holding no file, which sent the person reading the check
    /// to look for a file manager problem that was never there.
    #[tokio::test]
    async fn a_failing_file_reader_is_told_apart_from_an_empty_clipboard() {
        use super::{CommandSpec, read_reference_output};

        let quiet = CommandSpec::new("sh", &["-c", "printf ''"]);
        assert_eq!(read_reference_output(&quiet).await, Ok(String::new()));

        let named = CommandSpec::new("sh", &["-c", "printf 'file:///tmp/a.c\n'"]);
        assert_eq!(
            read_reference_output(&named).await.as_deref(),
            Ok("file:///tmp/a.c\n")
        );

        let failed = CommandSpec::new("sh", &["-c", "echo 'cannot coerce' >&2; exit 3"]);
        let complaint = read_reference_output(&failed).await.unwrap_err();
        assert!(
            complaint.contains("exit 3") && complaint.contains("cannot coerce"),
            "a failure should carry its own account, got {complaint:?}"
        );

        let missing = CommandSpec::new("super-herdr-no-such-reader", &[]);
        assert!(
            read_reference_output(&missing)
                .await
                .unwrap_err()
                .contains("could not start")
        );
    }

    /// The fallback exists because the first reader is not always the one that
    /// works. It is reached only when a reader *fails* — a reader that answers
    /// has answered, and asking the next one to disagree is how a clipboard
    /// holding text ends up described as holding a file.
    #[tokio::test]
    async fn a_reader_that_answers_ends_the_walk_and_a_reader_that_fails_does_not() {
        use super::{CommandSpec, probe_readers};

        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("a.c");
        let second = directory.path().join("b.c");
        std::fs::write(&first, b"int main(void) { return 0; }").unwrap();
        std::fs::write(&second, b"int other(void) { return 1; }").unwrap();

        let broken = || CommandSpec::new("sh", &["-c", "echo nope >&2; exit 1"]);
        let naming = |paths: &[&std::path::Path]| {
            let listing = paths
                .iter()
                .map(|path| format!("file://{}", path.display()))
                .collect::<Vec<_>>()
                .join("\n");
            CommandSpec::new(
                "sh",
                &[
                    "-c",
                    Box::leak(format!("printf '{listing}\n'").into_boxed_str()),
                ],
            )
        };

        // A failure hands over; the one that answers is the one reported.
        let probe = probe_readers(vec![
            ("first", broken()),
            ("second", naming(&[&first, &second])),
        ])
        .await;
        assert_eq!(probe.paths, vec![first.clone(), second.clone()]);
        assert!(probe.attempts[0].starts_with("first: failed (exit 1)"));
        assert_eq!(probe.attempts[1], "second: named 2 file(s)");

        // An answer ends the walk, so a second reader cannot overwrite it.
        let probe = probe_readers(vec![
            ("first", naming(&[&first])),
            ("second", naming(&[&second])),
        ])
        .await;
        assert_eq!(probe.paths, vec![first.clone()]);
        assert_eq!(probe.attempts, vec!["first: named 1 file(s)".to_owned()]);

        // Including when the answer is "nothing". This is the case that matters:
        // `the clipboard as «class furl»` coerces copied *text* into a file
        // reference, so a fallback consulted after a real answer of "no file"
        // would invent one.
        let empty = || CommandSpec::new("sh", &["-c", "printf ''"]);
        let probe = probe_readers(vec![("first", empty()), ("second", naming(&[&first]))]).await;
        assert!(probe.paths.is_empty());
        assert_eq!(
            probe.attempts,
            vec!["first: answered, naming no file on this disk".to_owned()]
        );
    }

    /// Named and present are different claims, and only one of them can be
    /// copied. A path that is not on this disk is a reader inventing a file
    /// rather than reporting one.
    #[tokio::test]
    async fn a_path_that_is_not_on_this_disk_is_not_a_copied_file() {
        use super::{CommandSpec, probe_readers};

        // What the macOS coercion actually produced from copied text: a
        // plausible absolute path naming nothing.
        let invented = CommandSpec::new(
            "sh",
            &["-c", "printf '/super-herdr clipboard check --wait 20\n'"],
        );

        let probe = probe_readers(vec![("coercion", invented)]).await;

        assert!(probe.paths.is_empty());
        assert_eq!(
            probe.attempts,
            vec!["coercion: answered, naming no file on this disk".to_owned()]
        );
    }

    /// A reader's complaint reaches a terminal, so it is bounded and stripped
    /// exactly like the flavor names are.
    #[test]
    fn a_readers_complaint_is_bounded_before_it_is_shown() {
        use super::{MAXIMUM_COMPLAINT_CHARS, reader_complaint};

        assert_eq!(
            reader_complaint(b"execution error: no\n", Some(1)),
            "failed (exit 1): execution error: no"
        );
        assert_eq!(reader_complaint(b"   \n", Some(1)), "failed (exit 1)");
        assert_eq!(reader_complaint(b"", None), "failed (killed by a signal)");
        // Escape sequences are not something a clipboard owner gets to write
        // to somebody else's terminal.
        assert_eq!(
            reader_complaint(b"\x1b[2Jgone", Some(1)),
            "failed (exit 1): [2Jgone"
        );
        let shouted = vec![b'x'; 4096];
        assert!(reader_complaint(&shouted, Some(1)).len() < MAXIMUM_COMPLAINT_CHARS + 40);
    }

    #[test]
    fn a_transfer_name_is_refused_rather_than_repaired() {
        // These remain one path component, and clients quote the verified path
        // before it is pasted into a terminal. Ordinary document names should
        // therefore stay ordinary rather than being rejected or renamed.
        for name in [
            "report.pdf",
            "Quarterly report (final).docx",
            "roadmap's figures.pptx",
            "naïve résumé.pdf",
            "build-log.txt",
            "core_dump",
            "v1.2.3-rc4.tar.gz",
            ".hidden",
            "-rf",
            "$(whoami).txt",
            "a",
        ] {
            assert!(
                StagedName::resolve(Some(name), OPAQUE).is_ok(),
                "{name:?} should be allowed"
            );
        }

        // What cannot be one safely framed path component is refused rather
        // than repaired or truncated.
        for name in [
            "../etc/passwd",  // leaves the staging directory
            "sub/dir.txt",    // same, by a shorter route
            "..",             // the directory itself
            ".",              //
            "two..dots.docx", // resume guards reject traversal-like names too
            "new\nline.txt",  // would break the framing outright
            "tab\there.pptx", // would corrupt the tab-separated receipt
            "",               // outside a class narrow enough to reason about
        ] {
            assert!(
                StagedName::resolve(Some(name), OPAQUE).is_err(),
                "{name:?} should be refused"
            );
        }

        // Longer than the ceiling, by one byte.
        let long = "a".repeat(super::MAX_TRANSFER_NAME_BYTES + 1);
        assert!(StagedName::resolve(Some(&long), OPAQUE).is_err());
        let limit = "a".repeat(super::MAX_TRANSFER_NAME_BYTES);
        assert!(StagedName::resolve(Some(&limit), OPAQUE).is_ok());

        // No name is not a refusal: it is the clipboard's case, and the flavor
        // names it.
        assert_eq!(
            StagedName::resolve(None, PNG).unwrap(),
            StagedName("payload.png".to_owned())
        );
        assert_eq!(
            StagedName::resolve(None, OPAQUE).unwrap(),
            StagedName("payload".to_owned())
        );
    }

    #[tokio::test]
    async fn a_named_transfer_is_staged_under_the_name_it_was_given() {
        let payload = vec![3u8; 4096];
        let uploaded = complete(
            upload_stream(
                &local_target(),
                &TransportConfig::default(),
                fresh(Some("release-notes.md"), OPAQUE, payload.len() as u64),
                payload.as_slice(),
            )
            .await
            .unwrap(),
        );
        assert!(
            uploaded.path.ends_with("/release-notes.md"),
            "{}",
            uploaded.path
        );
        assert_eq!(std::fs::read(&uploaded.path).unwrap(), payload);
        // The staging directory is the unit that goes, so a refused or
        // finished transfer leaves no empty directory behind either.
        discard_local_upload(std::path::Path::new(&uploaded.path));
        assert!(!std::path::Path::new(&uploaded.path).exists());
        assert!(
            !std::path::Path::new(&uploaded.path)
                .parent()
                .unwrap()
                .exists()
        );
    }

    #[tokio::test]
    async fn an_office_document_keeps_its_ordinary_filename() {
        let payload = b"opaque pptx bytes";
        let uploaded = complete(
            upload_stream(
                &local_target(),
                &TransportConfig::default(),
                fresh(
                    Some("Quarterly plan's (final).pptx"),
                    OPAQUE,
                    payload.len() as u64,
                ),
                payload.as_slice(),
            )
            .await
            .unwrap(),
        );
        assert!(
            uploaded.path.ends_with("/Quarterly plan's (final).pptx"),
            "{}",
            uploaded.path
        );
        assert_eq!(std::fs::read(&uploaded.path).unwrap(), payload);
        discard_local_upload(std::path::Path::new(&uploaded.path));
    }

    #[test]
    fn the_download_script_describes_then_sends_under_every_shell() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("read-me.bin");
        let contents = b"a file to be read back off a host".as_slice();
        std::fs::write(&source, contents).unwrap();

        let mut tried = 0;
        for shell in ["sh", "bash", "zsh", "dash", "ksh"] {
            let Ok(mut child) = Command::new(shell)
                .arg("-c")
                .arg(DOWNLOAD_SCRIPT)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };
            tried += 1;
            let mut input = child.stdin.take().unwrap();
            input
                .write_all(format!("{}\n", source.display()).as_bytes())
                .unwrap();
            drop(input);
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "the script failed under {shell}");

            // The header is one line and everything after it is the file. A
            // shell that framed that boundary differently is what this checks.
            let split = output
                .stdout
                .iter()
                .position(|byte| *byte == b'\n')
                .unwrap();
            let header = std::str::from_utf8(&output.stdout[..=split]).unwrap();
            let (length, digest) = super::parse_source_header(header).unwrap();
            assert_eq!(
                length as usize,
                contents.len(),
                "{shell} reported a bad size"
            );
            assert_eq!(
                digest,
                sha256_hex(contents),
                "{shell} reported a bad digest"
            );
            assert_eq!(
                &output.stdout[split + 1..],
                contents,
                "{shell} sent the wrong bytes"
            );
        }
        assert!(
            tried > 0,
            "no shell was available to run the download script"
        );

        // Anything that is not a readable regular file is refused before a byte
        // is sent: a directory would otherwise be a stream with no end.
        for path in [
            directory.path().display().to_string(),
            "/nonexistent/nothing-here".to_owned(),
            String::new(),
        ] {
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(DOWNLOAD_SCRIPT)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("sh is available");
            let mut input = child.stdin.take().unwrap();
            let _ = input.write_all(format!("{path}\n").as_bytes());
            drop(input);
            let output = child.wait_with_output().unwrap();
            assert!(!output.status.success(), "the script accepted {path:?}");
            assert!(output.stdout.is_empty(), "the script described {path:?}");
        }
    }

    #[tokio::test]
    async fn a_source_is_described_by_the_host_and_named_without_a_path() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("notes.txt");
        let contents = b"read this back".as_slice();
        std::fs::write(&source, contents).unwrap();

        let mut opened = open_source(
            &local_target(),
            &TransportConfig::default(),
            source.to_str().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(opened.name, "notes.txt", "a name, never a path");
        assert_eq!(opened.length as usize, contents.len());
        assert_eq!(opened.digest, sha256_hex(contents));

        let mut read = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut opened, &mut read)
            .await
            .unwrap();
        assert_eq!(read, contents);

        // A path that is not one line of ordinary text never reaches a host.
        for path in ["", "two\nlines", "bell\u{7}"] {
            assert!(
                open_source(&local_target(), &TransportConfig::default(), path)
                    .await
                    .is_err(),
                "{path:?} should be refused"
            );
        }
    }

    #[test]
    fn the_resume_script_appends_under_every_shell_and_refuses_a_path_it_did_not_stage() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let directory = tempfile::Builder::new()
            .prefix("super-herdr-clipboard.")
            .tempdir()
            .unwrap();
        let staged = directory.path().join("payload.bin");
        let mut tried = 0;
        for shell in ["sh", "bash", "zsh", "dash", "ksh"] {
            std::fs::write(&staged, b"first-half-").unwrap();
            let Ok(mut child) = Command::new(shell)
                .arg("-c")
                .arg(RESUME_SCRIPT)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };
            tried += 1;
            let mut input = child.stdin.take().unwrap();
            input
                .write_all(format!("{}\n", staged.display()).as_bytes())
                .unwrap();
            input.write_all(b"second-half").unwrap();
            drop(input);
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "the resume script failed under {shell}"
            );
            let receipt = parse_remote_upload_receipt(&output.stdout)
                .unwrap_or_else(|error| panic!("{shell} produced no usable receipt: {error}"));
            // Appended, not replaced, and the receipt describes the whole file
            // rather than what this attempt contributed.
            assert_eq!(
                std::fs::read(&staged).unwrap(),
                b"first-half-second-half",
                "{shell} did not append"
            );
            assert_eq!(receipt.bytes, 22, "{shell} reported the wrong total");
            assert_eq!(receipt.digest, sha256_hex(b"first-half-second-half"));
        }
        assert!(tried > 0, "no shell was available to run the resume script");

        // A path outside a staging directory is refused by the script itself,
        // whatever the daemon believed when it sent it.
        let outside = tempfile::NamedTempFile::new().unwrap();
        for path in [
            outside.path().display().to_string(),
            format!("{}/../escape", directory.path().display()),
            "/etc/passwd".to_owned(),
        ] {
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(RESUME_SCRIPT)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("sh is available");
            let mut input = child.stdin.take().unwrap();
            let _ = input.write_all(format!("{path}\nappended").as_bytes());
            drop(input);
            let output = child.wait_with_output().unwrap();
            assert!(!output.status.success(), "the script accepted {path:?}");
        }
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"");
    }

    /// The script refuses a separator even though nothing should ever send it
    /// one. Two independent checks is the point: this one holds if the
    /// daemon's is ever widened, and it is the only one that runs on the host
    /// where the file is actually written.
    #[test]
    fn the_upload_script_refuses_a_name_that_would_leave_its_directory() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        for name in ["../escape", "sub/dir", "..", ".", ""] {
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(UPLOAD_SCRIPT)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("sh is available");
            let mut input = child.stdin.take().unwrap();
            let _ = input.write_all(format!("{name}\npayload").as_bytes());
            drop(input);
            let output = child.wait_with_output().unwrap();
            assert!(
                !output.status.success(),
                "the script accepted the name {name:?}"
            );
            assert!(
                output.stdout.is_empty(),
                "the script staged something for the name {name:?}"
            );
        }
    }

    #[test]
    fn only_a_directory_this_bridge_made_is_ever_removed() {
        assert_eq!(
            removable_upload_directory("/tmp/super-herdr-clipboard.ab12cd34/payload.png"),
            Some("/tmp/super-herdr-clipboard.ab12cd34")
        );
        // A remote host that reports something else gets nothing removed.
        for path in [
            "/etc/payload.png",
            "/tmp/payload.png",
            "relative/super-herdr-clipboard.x/payload.png",
            "/super-herdr-clipboard.x-elsewhere/../etc/payload.png",
            "payload.png",
        ] {
            assert!(
                removable_upload_directory(path).is_none(),
                "would have removed {path:?}"
            );
        }
    }

    #[test]
    fn every_table_entry_carries_an_inert_extension() {
        for media in KNOWN_MEDIA {
            assert!(
                media.payload_name().is_ok(),
                "{} has an extension that could reach a shell",
                media.mime
            );
        }
    }

    #[test]
    fn an_extension_that_could_reach_a_shell_is_refused() {
        // The extension is interpolated into the remote upload script, so a
        // table entry carrying shell syntax must never render one.
        for extension in ["", "png; rm -rf /", "../etc", "p ng", "png\"", "$(id)"] {
            let media = ClipboardMedia {
                mime: "test/hostile",
                extension: Some(Box::leak(extension.to_owned().into_boxed_str())),
                signatures: &[],
            };
            assert!(
                media.payload_name().is_err(),
                "accepted hostile extension {extension:?}"
            );
        }
        assert_eq!(PNG.payload_name().unwrap(), "payload.png");
    }
}
