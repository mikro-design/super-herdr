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

pub struct UploadedImage {
    pub path: String,
    pub bytes: usize,
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

#[derive(Debug, Clone, Copy)]
struct CommandSpec {
    program: &'static str,
    arguments: &'static [&'static str],
}

pub fn diagnostic_lines() -> Vec<String> {
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
    let image_method = if context == ClipboardContext::Desktop {
        image_reader_label()
    } else {
        "unavailable to an SSH/nested process".to_owned()
    };

    vec![
        format!("context: {}", context.label()),
        format!("copy: {copy_method} ({copy_acknowledgement})"),
        format!("paste action: {paste_method}"),
        format!("image paste action: {image_method}"),
        "clipboard payloads are neither inspected by this check nor written to logs".to_owned(),
    ]
}

pub fn write_text(text: &str) -> Result<ClipboardDelivery> {
    if !prefers_terminal_copy()
        && native_writer().is_some_and(|spec| run_writer(spec, text.as_bytes()))
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

pub async fn read_png(maximum_bytes: usize) -> Result<Vec<u8>> {
    if clipboard_context() != ClipboardContext::Desktop {
        bail!(
            "local clipboard image reads are unavailable over SSH or inside Herdr; run Super-Herdr on the desktop"
        );
    }
    #[cfg(target_os = "linux")]
    {
        let Some(spec) = native_image_reader() else {
            bail!("no native PNG clipboard reader is available");
        };
        let bytes = read_command_bytes(spec, maximum_bytes, "clipboard image").await?;
        validate_png(&bytes)?;
        Ok(bytes)
    }
    #[cfg(target_os = "macos")]
    {
        read_macos_png(maximum_bytes).await
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = maximum_bytes;
        bail!("clipboard image reads are unsupported on this platform")
    }
}

pub async fn upload_png(
    target: &Target,
    transport: &TransportConfig,
    bytes: &[u8],
) -> Result<UploadedImage> {
    validate_png(bytes)?;
    let expected_digest = sha256_hex(bytes);
    if let Some(destination) = target.ssh.as_deref() {
        let upload = timeout(
            Duration::from_secs(transport.command_timeout_seconds),
            upload_remote_png(destination, transport, bytes),
        )
        .await
        .context("clipboard image upload timed out")??;
        if upload.bytes != bytes.len() || upload.digest != expected_digest {
            bail!("remote clipboard image verification failed");
        }
        return Ok(UploadedImage {
            path: upload.path,
            bytes: upload.bytes,
        });
    }
    upload_local_png(bytes, &expected_digest)
}

async fn read_command_bytes(
    spec: CommandSpec,
    maximum_bytes: usize,
    description: &str,
) -> Result<Vec<u8>> {
    let mut child = tokio::process::Command::new(spec.program)
        .args(spec.arguments)
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
        return Some(CommandSpec {
            program: "pbcopy",
            arguments: &[],
        });
    }
    #[cfg(target_os = "linux")]
    {
        if env::var_os("WAYLAND_DISPLAY").is_some() && command_available("wl-copy") {
            return Some(CommandSpec {
                program: "wl-copy",
                arguments: &["--type", "text/plain;charset=utf-8"],
            });
        }
        if env::var_os("DISPLAY").is_some() && command_available("xclip") {
            return Some(CommandSpec {
                program: "xclip",
                arguments: &["-selection", "clipboard", "-in"],
            });
        }
        if env::var_os("DISPLAY").is_some() && command_available("xsel") {
            return Some(CommandSpec {
                program: "xsel",
                arguments: &["--clipboard", "--input"],
            });
        }
    }
    None
}

fn native_reader() -> Option<CommandSpec> {
    #[cfg(target_os = "macos")]
    if command_available("pbpaste") {
        return Some(CommandSpec {
            program: "pbpaste",
            arguments: &[],
        });
    }
    #[cfg(target_os = "linux")]
    {
        if env::var_os("WAYLAND_DISPLAY").is_some() && command_available("wl-paste") {
            return Some(CommandSpec {
                program: "wl-paste",
                arguments: &["--no-newline"],
            });
        }
        if env::var_os("DISPLAY").is_some() && command_available("xclip") {
            return Some(CommandSpec {
                program: "xclip",
                arguments: &["-selection", "clipboard", "-out"],
            });
        }
        if env::var_os("DISPLAY").is_some() && command_available("xsel") {
            return Some(CommandSpec {
                program: "xsel",
                arguments: &["--clipboard", "--output"],
            });
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn native_image_reader() -> Option<CommandSpec> {
    if env::var_os("WAYLAND_DISPLAY").is_some() && command_available("wl-paste") {
        return Some(CommandSpec {
            program: "wl-paste",
            arguments: &["--type", "image/png"],
        });
    }
    if env::var_os("DISPLAY").is_some() && command_available("xclip") {
        return Some(CommandSpec {
            program: "xclip",
            arguments: &["-selection", "clipboard", "-type", "image/png", "-out"],
        });
    }
    None
}

fn image_reader_label() -> String {
    #[cfg(target_os = "linux")]
    {
        native_image_reader().map_or_else(
            || "unavailable (install wl-clipboard or xclip)".to_owned(),
            |reader| format!("native PNG ({})", reader.program),
        )
    }
    #[cfg(target_os = "macos")]
    {
        if command_available("osascript") {
            "native PNG (macOS clipboard via osascript)".to_owned()
        } else {
            "unavailable (osascript not found)".to_owned()
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        "unsupported on this platform".to_owned()
    }
}

#[cfg(target_os = "macos")]
async fn read_macos_png(maximum_bytes: usize) -> Result<Vec<u8>> {
    let output = tempfile::Builder::new()
        .prefix("super-herdr-clipboard-")
        .suffix(".png")
        .tempfile()
        .context("failed to create a temporary clipboard image file")?;
    let path = output.path().to_owned();
    let script = r#"
set outputPath to system attribute "SUPER_HERDR_CLIPBOARD_IMAGE"
set pngData to the clipboard as «class PNGf»
set outputFile to open for access POSIX file outputPath with write permission
try
    set eof outputFile to 0
    write pngData to outputFile
on error messageText number messageNumber
    close access outputFile
    error messageText number messageNumber
end try
close access outputFile
"#;
    let status = timeout(
        CLIPBOARD_COMMAND_TIMEOUT,
        tokio::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .env("SUPER_HERDR_CLIPBOARD_IMAGE", &path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .context("macOS clipboard image read timed out")?
    .context("failed to run the macOS clipboard image reader")?;
    if !status.success() {
        bail!("macOS clipboard does not contain a PNG-compatible image");
    }
    let metadata = fs::metadata(&path).context("failed to inspect the clipboard image")?;
    if metadata.len() > u64::try_from(maximum_bytes).unwrap_or(u64::MAX) {
        bail!("clipboard image exceeds the {maximum_bytes}-byte limit");
    }
    let bytes = fs::read(path).context("failed to read the clipboard image")?;
    validate_png(&bytes)?;
    Ok(bytes)
}

struct RemoteUploadReceipt {
    path: String,
    bytes: usize,
    digest: String,
}

async fn upload_remote_png(
    destination: &str,
    transport: &TransportConfig,
    bytes: &[u8],
) -> Result<RemoteUploadReceipt> {
    const SCRIPT: &str = r#"set -eu
umask 077
base=${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}
dir=$(mktemp -d "$base/super-herdr-clipboard.XXXXXXXX")
path="$dir/image.png"
cat > "$path"
size=$(wc -c < "$path" | tr -d '[:space:]')
digest=$(sha256sum "$path" | awk '{print $1}')
printf '%s\t%s\t%s\n' "$path" "$size" "$digest"
"#;
    let mut command = build_ssh_command(destination, transport, SCRIPT.to_owned());
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start SSH clipboard image upload")?;
    let mut input = child
        .stdin
        .take()
        .context("SSH clipboard image upload did not expose stdin")?;
    let output = child
        .stdout
        .take()
        .context("SSH clipboard image upload did not expose stdout")?;
    input
        .write_all(bytes)
        .await
        .context("failed to upload clipboard image bytes")?;
    input
        .shutdown()
        .await
        .context("failed to finish clipboard image upload")?;
    let mut receipt = Vec::new();
    output
        .take(4097)
        .read_to_end(&mut receipt)
        .await
        .context("failed to read the SSH clipboard image upload receipt")?;
    if receipt.len() > 4096 {
        let _ = child.kill().await;
        bail!("SSH clipboard image upload returned an oversized receipt");
    }
    let status = child
        .wait()
        .await
        .context("failed to wait for SSH clipboard image upload")?;
    if !status.success() {
        bail!("SSH clipboard image upload failed (diagnostics redacted)");
    }
    parse_remote_upload_receipt(&receipt)
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
        bail!("remote clipboard image upload returned an invalid receipt");
    }
    Ok(RemoteUploadReceipt {
        path: path.to_owned(),
        bytes: size
            .parse()
            .context("remote clipboard image upload returned an invalid size")?,
        digest: digest.to_ascii_lowercase(),
    })
}

fn upload_local_png(bytes: &[u8], expected_digest: &str) -> Result<UploadedImage> {
    let mut file = tempfile::Builder::new()
        .prefix("super-herdr-clipboard-")
        .suffix(".png")
        .tempfile()
        .context("failed to create a local clipboard image file")?;
    file.write_all(bytes)
        .context("failed to write the local clipboard image")?;
    file.flush()
        .context("failed to flush the local clipboard image")?;
    let (_, path) = file
        .keep()
        .context("failed to retain the local clipboard image")?;
    let verified = fs::read(&path).context("failed to verify the local clipboard image")?;
    if verified.len() != bytes.len() || sha256_hex(&verified) != expected_digest {
        bail!("local clipboard image verification failed");
    }
    Ok(UploadedImage {
        path: path.display().to_string(),
        bytes: bytes.len(),
    })
}

fn validate_png(bytes: &[u8]) -> Result<()> {
    const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if !bytes.starts_with(PNG_SIGNATURE) {
        bail!("clipboard does not contain a PNG image");
    }
    Ok(())
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

fn run_writer(spec: CommandSpec, bytes: &[u8]) -> bool {
    let Ok(mut child) = Command::new(spec.program)
        .args(spec.arguments)
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

    use super::{ClipboardContext, parse_remote_upload_receipt, sha256_hex, validate_png};

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
    fn validates_png_and_remote_size_digest_receipts() {
        let png = b"\x89PNG\r\n\x1a\nbody";
        assert!(validate_png(png).is_ok());
        assert!(validate_png(b"not png").is_err());
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
}
