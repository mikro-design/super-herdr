use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use crate::config::{Target, TransportConfig};
use crate::transport::build_ssh_command;

const CLIPBOARD_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardDelivery {
    Native,
    Osc52Requested,
}

pub struct UploadedFile {
    pub path: String,
    pub bytes: usize,
    pub mime: &'static str,
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
    /// Extension given to the uploaded file. It always comes from this table
    /// and never from the clipboard, so no untrusted text reaches the remote
    /// command.
    pub extension: &'static str,
    /// Alternatives that identify the format, any one of which is enough. Each
    /// alternative is a set of markers that must all match, because a format
    /// like WebP is only identified by two patterns at different offsets, and
    /// one like GIF has more than one valid form. Empty means the format has no
    /// signature worth checking.
    signatures: &'static [&'static [Marker]],
}

pub const PNG: ClipboardMedia = ClipboardMedia {
    mime: "image/png",
    extension: "png",
    signatures: &[&[Marker {
        offset: 0,
        bytes: b"\x89PNG\r\n\x1a\n",
    }]],
};

pub const JPEG: ClipboardMedia = ClipboardMedia {
    mime: "image/jpeg",
    extension: "jpg",
    signatures: &[&[Marker {
        offset: 0,
        bytes: b"\xff\xd8\xff",
    }]],
};

/// RIFF alone also begins AVI and WAV, so the container tag at offset 8 is what
/// actually identifies WebP.
pub const WEBP: ClipboardMedia = ClipboardMedia {
    mime: "image/webp",
    extension: "webp",
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
    extension: "gif",
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
    extension: "tif",
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
    extension: "pdf",
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
    extension: "svg",
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

    /// The extension reaches a remote shell command, so it must stay inert.
    fn safe_extension(self) -> Result<&'static str> {
        if self.extension.is_empty() || !self.extension.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("unsupported clipboard media extension");
        }
        Ok(self.extension)
    }
}

/// Flavors the bridge can move, in the order they are preferred when a
/// clipboard offers several at once. PNG stays first so that the common case,
/// a screenshot, keeps behaving exactly as it always has.
pub const KNOWN_MEDIA: &[ClipboardMedia] = &[PNG, JPEG, WEBP, GIF, TIFF, PDF, SVG];

/// A clipboard type list is metadata, not payload, but it is still written by
/// whichever program owns the clipboard, so it is bounded before it is read.
const MAXIMUM_TYPE_LIST_BYTES: usize = 64 * 1024;
const MAXIMUM_REPORTED_TYPES: usize = 12;
const MAXIMUM_TYPE_NAME_CHARS: usize = 80;

pub async fn diagnostic_lines() -> Vec<String> {
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

    vec![
        format!("context: {}", context.label()),
        format!("copy: {copy_method} ({copy_acknowledgement})"),
        format!("paste action: {paste_method}"),
        format!("media paste action: {media_method}"),
        format!("clipboard offers: {offered}"),
        format!(
            "uploadable flavors: {}",
            KNOWN_MEDIA
                .iter()
                .map(|media| media.mime)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        "clipboard payloads are neither inspected by this check nor written to logs".to_owned(),
    ]
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
    let expected_digest = sha256_hex(bytes);
    if let Some(destination) = target.ssh.as_deref() {
        let upload = timeout(
            Duration::from_secs(transport.command_timeout_seconds),
            upload_remote_media(destination, transport, media, bytes),
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
        });
    }
    upload_local_media(media, bytes, &expected_digest)
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
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
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
                    .find(|(_, code)| code.trim_end() == class)
                    .map_or_else(|| format!("class:{class}"), |(mime, _)| (*mime).to_owned()),
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
        .suffix(&format!(".{}", media.safe_extension()?))
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

async fn upload_remote_media(
    destination: &str,
    transport: &TransportConfig,
    media: ClipboardMedia,
    bytes: &[u8],
) -> Result<RemoteUploadReceipt> {
    // The extension is the only part of this script that is not a literal, and
    // it is checked to be alphanumeric first, so the command still carries no
    // clipboard-derived text.
    let script = format!(
        r#"set -eu
umask 077
base=${{XDG_RUNTIME_DIR:-${{TMPDIR:-/tmp}}}}
dir=$(mktemp -d "$base/super-herdr-clipboard.XXXXXXXX")
path="$dir/payload.{extension}"
cat > "$path"
size=$(wc -c < "$path" | tr -d '[:space:]')
digest=$(sha256sum "$path" | awk '{{print $1}}')
printf '%s\t%s\t%s\n' "$path" "$size" "$digest"
"#,
        extension = media.safe_extension()?
    );
    let mut command = build_ssh_command(destination, transport, script);
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
    input
        .write_all(bytes)
        .await
        .context("failed to upload clipboard media bytes")?;
    input
        .shutdown()
        .await
        .context("failed to finish clipboard media upload")?;
    let mut receipt = Vec::new();
    output
        .take(4097)
        .read_to_end(&mut receipt)
        .await
        .context("failed to read the SSH clipboard media upload receipt")?;
    if receipt.len() > 4096 {
        let _ = child.kill().await;
        bail!("SSH clipboard media upload returned an oversized receipt");
    }
    let status = child
        .wait()
        .await
        .context("failed to wait for SSH clipboard media upload")?;
    if !status.success() {
        bail!("SSH clipboard media upload failed (diagnostics redacted)");
    }
    parse_remote_upload_receipt(&receipt)
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
IFS= read -r dir
case "$dir" in
  /*/super-herdr-clipboard.*) ;;
  *) exit 1 ;;
esac
rm -rf -- "$dir"
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
    bytes: &[u8],
    expected_digest: &str,
) -> Result<UploadedFile> {
    let mut file = tempfile::Builder::new()
        .prefix("super-herdr-clipboard-")
        .suffix(&format!(".{}", media.safe_extension()?))
        .tempfile()
        .context("failed to create a local clipboard media file")?;
    file.write_all(bytes)
        .context("failed to write the local clipboard media")?;
    file.flush()
        .context("failed to flush the local clipboard media")?;
    let (_, path) = file
        .keep()
        .context("failed to retain the local clipboard media")?;
    let verified = fs::read(&path).context("failed to verify the local clipboard media")?;
    if verified.len() != bytes.len() || sha256_hex(&verified) != expected_digest {
        let _ = fs::remove_file(&path);
        bail!("local clipboard media verification failed");
    }
    Ok(UploadedFile {
        path: path.display().to_string(),
        bytes: bytes.len(),
        mime: media.mime,
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
        ClipboardContext, ClipboardMedia, GIF, JPEG, KNOWN_MEDIA, PDF, PNG, SVG, TIFF, WEBP,
        parse_remote_upload_receipt, removable_upload_directory, sha256_hex,
    };

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
            extension: "bin",
            signatures: &[],
        };
        assert!(OPAQUE.validate(b"anything at all").is_ok());
        assert_eq!(OPAQUE.safe_extension().unwrap(), "bin");
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
                media.safe_extension().is_ok(),
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
                extension: Box::leak(extension.to_owned().into_boxed_str()),
                signatures: &[],
            };
            assert!(
                media.safe_extension().is_err(),
                "accepted hostile extension {extension:?}"
            );
        }
        assert!(PNG.safe_extension().is_ok());
    }
}
