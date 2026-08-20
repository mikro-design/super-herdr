//! Exercise the verified upload path against a real SSH target.
//!
//! Unit tests cover the local sink and the guards; nothing proves the remote
//! half without a host to send to. This drives it end to end: a clipboard-sized
//! payload, a streamed one, and a refusal, checking after the refusal that
//! nothing was left staged on the host.
//!
//! Usage: cargo run --example qualify-upload -- <ssh-destination>

use anyhow::{Context, Result};
use super_herdr::clipboard::{self, PNG};
use super_herdr::config::{Target, TransportConfig};

fn png(bytes: usize) -> Vec<u8> {
    let mut payload = b"\x89PNG\r\n\x1a\n".to_vec();
    payload.resize(bytes, 0x5a);
    payload
}

fn remote(destination: &str, transport: &TransportConfig, script: &str) -> Result<String> {
    let output = std::process::Command::new(&transport.ssh_bin)
        .args(["-o", "BatchMode=yes", destination, script])
        .output()
        .context("failed to run the remote check")?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[tokio::main]
async fn main() -> Result<()> {
    let destination = std::env::args()
        .nth(1)
        .context("usage: <ssh-destination>")?;
    let transport = TransportConfig::default();
    let target = Target {
        name: "qualify".to_owned(),
        ssh: Some(destination.clone()),
        discover_sessions: false,
        session: None,
        socket: None,
        herdr_bins: vec!["herdr".to_owned()],
    };

    let payload = png(64 * 1024);
    let uploaded = clipboard::upload_media(&target, &transport, PNG, &payload).await?;
    let stored = remote(
        &destination,
        &transport,
        &format!("sha256sum {} | cut -d' ' -f1", uploaded.path),
    )?;
    println!(
        "upload_media: {} bytes -> {}",
        uploaded.bytes, uploaded.path
    );
    println!(
        "  remote digest matches source: {}",
        stored == sha256(&payload)
    );

    let streamed = png(3 * 1024 * 1024);
    let sent = clipboard::upload_stream(
        &target,
        &transport,
        PNG,
        streamed.as_slice(),
        streamed.len() as u64,
    )
    .await?;
    let stored = remote(
        &destination,
        &transport,
        &format!("sha256sum {} | cut -d' ' -f1", sent.path),
    )?;
    println!("upload_stream: {} bytes -> {}", sent.bytes, sent.path);
    println!(
        "  remote digest matches source: {}",
        stored == sha256(&streamed)
    );

    // A source shorter than its declared length must be refused, and the
    // refusal must take the staged payload with it.
    let short = png(1024);
    let before = remote(
        &destination,
        &transport,
        "ls -d ${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/super-herdr-clipboard.* 2>/dev/null | wc -l",
    )?;
    let error = clipboard::upload_stream(&target, &transport, PNG, short.as_slice(), 8192)
        .await
        .expect_err("a short source must be refused");
    let after = remote(
        &destination,
        &transport,
        "ls -d ${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/super-herdr-clipboard.* 2>/dev/null | wc -l",
    )?;
    println!("refusal: {error}");
    println!("  staged directories before {before}, after {after} (equal means nothing was left)");

    for path in [uploaded.path, sent.path] {
        let _ = remote(
            &destination,
            &transport,
            &format!("rm -rf -- $(dirname {path})"),
        );
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
