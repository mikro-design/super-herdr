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
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
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

    /// The name of the file this flavor is written to.
    ///
    /// The extension reaches a remote shell command, so it must stay inert; a
    /// flavor that declares none is written as a bare `payload`.
    fn payload_name(self) -> Result<String> {
        let Some(extension) = self.extension else {
            return Ok("payload".to_owned());
        };
        if extension.is_empty() || !extension.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("unsupported clipboard media extension");
        }
        Ok(format!("payload.{extension}"))
    }

    /// The suffix a local temporary file is given, if any.
    fn file_suffix(self) -> Result<String> {
        Ok(match self.payload_name()?.split_once('.') {
            Some((_, extension)) => format!(".{extension}"),
            None => String::new(),
        })
    }
}

/// A payload whose type this bridge does not recognize.
///
/// It carries no extension, because the only safe name is no name. A name from
/// the wire would be attacker-influenced text in a remote command, and a
/// sanitizer that has to stay correct forever is exactly the thing someone
/// widens later under pressure. The byte count and digest are what prove the
/// transfer; a display name belongs to whichever client can still read it.
pub const OPAQUE: ClipboardMedia = ClipboardMedia {
    mime: "application/octet-stream",
    extension: None,
    signatures: &[],
};

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
    media: ClipboardMedia,
    source: R,
    expected_bytes: u64,
) -> Result<UploadedFile>
where
    R: AsyncRead + Unpin,
{
    if let Some(destination) = target.ssh.as_deref() {
        let (receipt, digest, written) =
            upload_remote_stream(destination, transport, media, source, expected_bytes).await?;
        let verdict = verify_transfer(&receipt, &digest, written, expected_bytes);
        if let Err(error) = verdict {
            remove_remote_upload(destination, transport, &receipt.path).await;
            return Err(error);
        }
        return Ok(UploadedFile {
            path: receipt.path,
            bytes: receipt.bytes,
            mime: media.mime,
        });
    }
    upload_local_stream(media, source, expected_bytes).await
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
    let name = path.rsplit_once('/').map_or(path, |(_, tail)| tail);
    if name.starts_with("super-herdr-clipboard-") {
        let _ = fs::remove_file(path);
    }
}

/// Compare a receipt against what was actually sent, naming the failed check.
fn verify_transfer(
    receipt: &RemoteUploadReceipt,
    digest: &str,
    written: u64,
    expected_bytes: u64,
) -> Result<()> {
    if written != expected_bytes {
        bail!("transfer ended after {written} of {expected_bytes} declared bytes");
    }
    if receipt.bytes as u64 != written {
        bail!(
            "host stored {} bytes of the {written} that were sent",
            receipt.bytes
        );
    }
    if receipt.digest != digest {
        bail!("stored payload does not match the digest of the bytes that were sent");
    }
    Ok(())
}

async fn upload_remote_stream<R>(
    destination: &str,
    transport: &TransportConfig,
    media: ClipboardMedia,
    mut source: R,
    expected_bytes: u64,
) -> Result<(RemoteUploadReceipt, String, u64)>
where
    R: AsyncRead + Unpin,
{
    let mut command = build_ssh_command(destination, transport, upload_script(media)?);
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

    let mut hasher = Sha256::new();
    let mut written = 0u64;
    let mut buffer = vec![0u8; STREAM_CHUNK_BYTES];
    // A failure moving the bytes is recorded rather than returned. By the time
    // one can happen the host has already staged part of a file, and the only
    // way to learn where is to finish the exchange and read the receipt.
    // Returning early would strand exactly the artifact a refusal must not
    // leave behind.
    let mut failure: Option<anyhow::Error> = None;
    while written < expected_bytes {
        let wanted = usize::try_from(expected_bytes - written)
            .unwrap_or(STREAM_CHUNK_BYTES)
            .min(STREAM_CHUNK_BYTES);
        match source.read(&mut buffer[..wanted]).await {
            Ok(0) => break,
            Ok(read) => {
                hasher.update(&buffer[..read]);
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
    if let Some(error) = failure {
        if let Ok(receipt) = &receipt {
            remove_remote_upload(destination, transport, &receipt.path).await;
        }
        return Err(error);
    }
    let status = waited.context("failed to wait for the SSH media upload")?;
    if !status.success() {
        bail!("SSH media upload failed (diagnostics redacted)");
    }
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok((receipt?, digest, written))
}

async fn upload_local_stream<R>(
    media: ClipboardMedia,
    mut source: R,
    expected_bytes: u64,
) -> Result<UploadedFile>
where
    R: AsyncRead + Unpin,
{
    let file = tempfile::Builder::new()
        .prefix("super-herdr-clipboard-")
        .suffix(&media.file_suffix()?)
        .tempfile()
        .context("failed to create a local media file")?;
    let (_, path) = file
        .keep()
        .context("failed to retain the local media file")?;
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    {
        let mut sink = fs::File::create(&path).context("failed to open the local media file")?;
        let mut buffer = vec![0u8; STREAM_CHUNK_BYTES];
        while written < expected_bytes {
            let wanted = usize::try_from(expected_bytes - written)
                .unwrap_or(STREAM_CHUNK_BYTES)
                .min(STREAM_CHUNK_BYTES);
            let read = source
                .read(&mut buffer[..wanted])
                .await
                .context("failed to read the upload source")?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            sink.write_all(&buffer[..read])
                .context("failed to write the local media file")?;
            written += read as u64;
        }
        sink.flush()
            .context("failed to flush the local media file")?;
    }
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let stored = fs::read(&path).context("failed to verify the local media file")?;
    let receipt = RemoteUploadReceipt {
        path: path.display().to_string(),
        bytes: stored.len(),
        digest: sha256_hex(&stored),
    };
    if let Err(error) = verify_transfer(&receipt, &digest, written, expected_bytes) {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    Ok(UploadedFile {
        path: receipt.path,
        bytes: receipt.bytes,
        mime: media.mime,
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
fn upload_script(media: ClipboardMedia) -> Result<String> {
    // The remote login shell runs this, and zsh ties several lowercase names to
    // its own variables — `path` is tied to PATH, so assigning it replaces the
    // command search path with one file and everything after it fails to be
    // found. That is the default shell on macOS, so the names here are chosen
    // to collide with nothing.
    Ok(format!(
        r#"set -eu
umask 077
staging_base=${{XDG_RUNTIME_DIR:-${{TMPDIR:-/tmp}}}}
staging_dir=$(mktemp -d "$staging_base/super-herdr-clipboard.XXXXXXXX")
staged_file="$staging_dir/{name}"
cat > "$staged_file"
staged_size=$(wc -c < "$staged_file" | tr -d '[:space:]')
staged_digest=$(sha256sum "$staged_file" | awk '{{print $1}}')
printf '%s\t%s\t%s\n' "$staged_file" "$staged_size" "$staged_digest"
"#,
        name = media.payload_name()?
    ))
}

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
    media: ClipboardMedia,
    bytes: &[u8],
) -> Result<RemoteUploadReceipt> {
    let mut command = build_ssh_command(destination, transport, upload_script(media)?);
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
    bytes: &[u8],
    expected_digest: &str,
) -> Result<UploadedFile> {
    let mut file = tempfile::Builder::new()
        .prefix("super-herdr-clipboard-")
        .suffix(&media.file_suffix()?)
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
        ClipboardContext, ClipboardMedia, GIF, JPEG, KNOWN_MEDIA, OPAQUE, PDF, PNG,
        RemoteUploadReceipt, SVG, TIFF, WEBP, media_for_mime, parse_remote_upload_receipt,
        removable_upload_directory, sha256_hex, upload_stream, verify_transfer,
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

    #[tokio::test]
    async fn a_streamed_upload_is_verified_against_the_bytes_that_were_sent() {
        let payload = vec![7u8; 200_000];
        let uploaded = upload_stream(
            &local_target(),
            &TransportConfig::default(),
            OPAQUE,
            payload.as_slice(),
            payload.len() as u64,
        )
        .await
        .unwrap();
        let stored = std::fs::read(&uploaded.path).unwrap();
        assert_eq!(stored, payload);
        assert_eq!(uploaded.bytes, payload.len());
        // An unrecognized type is written with no extension at all.
        assert!(uploaded.path.ends_with("payload") || !uploaded.path.contains('.'));
        let _ = std::fs::remove_file(&uploaded.path);
    }

    #[tokio::test]
    async fn a_short_source_is_refused_as_truncated() {
        let payload = vec![1u8; 1024];
        let error = upload_stream(
            &local_target(),
            &TransportConfig::default(),
            PNG,
            payload.as_slice(),
            4096,
        )
        .await
        .unwrap_err()
        .to_string();
        // The refusal names the check that failed, because a short read is the
        // one worth retrying.
        assert!(error.contains("1024 of 4096"), "{error}");
    }

    #[tokio::test]
    async fn a_source_longer_than_declared_is_cut_off_rather_than_believed() {
        let payload = vec![9u8; 10_000];
        let uploaded = upload_stream(
            &local_target(),
            &TransportConfig::default(),
            OPAQUE,
            payload.as_slice(),
            4096,
        )
        .await
        .unwrap();
        // Only the declared length reaches the host: a lying length must not
        // write unbounded data there.
        assert_eq!(uploaded.bytes, 4096);
        let _ = std::fs::remove_file(&uploaded.path);
    }

    #[test]
    fn each_refusal_names_the_check_that_failed() {
        // Only a short read is likely to be a dropped connection worth
        // retrying, so the three refusals must be distinguishable.
        let receipt = |bytes, digest: &str| RemoteUploadReceipt {
            path: "/tmp/super-herdr-clipboard.aa/payload".to_owned(),
            bytes,
            digest: digest.to_owned(),
        };
        let truncated = verify_transfer(&receipt(10, "abc"), "abc", 10, 20).unwrap_err();
        assert!(truncated.to_string().contains("10 of 20"), "{truncated}");

        let short_write = verify_transfer(&receipt(5, "abc"), "abc", 10, 10).unwrap_err();
        assert!(
            short_write.to_string().contains("host stored 5"),
            "{short_write}"
        );

        let corrupt = verify_transfer(&receipt(10, "zzz"), "abc", 10, 10).unwrap_err();
        assert!(corrupt.to_string().contains("digest"), "{corrupt}");

        assert!(verify_transfer(&receipt(10, "abc"), "abc", 10, 10).is_ok());
    }

    #[test]
    fn an_unknown_type_is_carried_opaquely_rather_than_refused() {
        assert_eq!(media_for_mime("image/png"), PNG);
        assert_eq!(media_for_mime("application/x-anything"), OPAQUE);
        assert_eq!(media_for_mime("").extension, None);
        assert_eq!(OPAQUE.payload_name().unwrap(), "payload");
        assert_eq!(OPAQUE.file_suffix().unwrap(), "");
        assert!(OPAQUE.validate(b"\x00\x01\x02").is_ok());
    }

    #[test]
    fn the_upload_script_runs_under_every_shell_a_host_might_use() {
        // The remote login shell runs this script, and it is not necessarily
        // the one running the tests. zsh ties `path` to PATH, so an earlier
        // version replaced the command search path with the staged file and
        // every command after it vanished — invisible to a unit test that only
        // ever tried one shell, and fatal on macOS, where zsh is the default.
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let script = super::upload_script(PNG).unwrap();
        let payload = b"\x89PNG\r\n\x1a\nqualification";
        let mut tried = 0;
        for shell in ["sh", "bash", "zsh", "dash", "ksh"] {
            // A shell that is not installed simply fails to spawn and is
            // skipped; the assertion at the end catches a host with none.
            let Ok(mut child) = Command::new(shell)
                .arg("-c")
                .arg(&script)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };
            tried += 1;
            child.stdin.take().unwrap().write_all(payload).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "the upload script failed under {shell}"
            );
            let receipt = parse_remote_upload_receipt(&output.stdout)
                .unwrap_or_else(|error| panic!("{shell} produced no usable receipt: {error}"));
            assert_eq!(
                receipt.bytes,
                payload.len(),
                "{shell} stored the wrong size"
            );
            assert_eq!(
                receipt.digest,
                sha256_hex(payload),
                "{shell} stored wrong bytes"
            );
            let _ = std::fs::remove_dir_all(std::path::Path::new(&receipt.path).parent().unwrap());
        }
        assert!(tried > 0, "no shell was available to run the upload script");
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
