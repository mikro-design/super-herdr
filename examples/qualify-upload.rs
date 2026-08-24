//! Exercise the verified upload path against a real SSH target.
//!
//! Unit tests cover the local sink and the guards; nothing proves the remote
//! half without a host to send to. This drives it end to end: a clipboard-sized
//! payload, a streamed one, and a refusal, checking after the refusal that
//! nothing was left staged on the host.
//!
//! The daemon's relay is qualified here too. Its buffering and verification are
//! unit tested and its sink is qualified above, but the join between them —
//! protocol chunks reassembled, handed to the remote upload, landing intact on
//! another host — is the same never-run shape, and a composition of two tested
//! halves is not itself a tested path.
//!
//! Usage: cargo run --example qualify-upload -- <ssh-destination> [session] [socket]
//!
//! The relay scenario needs a session and socket as well, because an upload
//! requires the pane's control lease and a lease requires a terminal route that
//! opens. Given only a destination, the clipboard scenarios run and the relay
//! says why it was skipped.

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
    let sent = match clipboard::upload_stream(
        &target,
        &transport,
        clipboard::TransferPlan {
            media: PNG,
            staging: clipboard::Staging::Fresh {
                name: Some("qualification-streamed.png"),
            },
            length: streamed.len() as u64,
        },
        streamed.as_slice(),
    )
    .await?
    {
        clipboard::Transferred::Complete(uploaded) => uploaded,
        clipboard::Transferred::Interrupted { staged, .. } => {
            anyhow::bail!("the streamed upload stopped after {staged} bytes")
        }
    };
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

    // A source that stops short keeps what arrived, and a second attempt
    // continues from wherever the host actually got to. Both halves are
    // qualified here because neither is provable without a real host: the
    // offset comes from the far side's own count, and the digest that verifies
    // the result spans two SSH connections.
    let whole = png(2 * 1024 * 1024);
    let staged_count = "ls -d ${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/super-herdr-clipboard.* \
                        2>/dev/null | wc -l";
    let before = remote(&destination, &transport, staged_count)?;
    let interrupted = clipboard::upload_stream(
        &target,
        &transport,
        clipboard::TransferPlan {
            media: PNG,
            staging: clipboard::Staging::Fresh {
                name: Some("qualification-resumed.png"),
            },
            length: whole.len() as u64,
        },
        &whole[..700_000],
    )
    .await?;
    let clipboard::Transferred::Interrupted { path, staged } = interrupted else {
        anyhow::bail!("a source that stopped short must not report a finished transfer");
    };
    println!(
        "interrupted: {staged} of {} bytes staged at {path}",
        whole.len()
    );

    let offset = clipboard::staged_bytes(&target, &transport, &path)
        .await
        .context("the host could not say how much it holds")?;
    println!("  host reports {offset} bytes staged");
    let resumed = clipboard::upload_stream(
        &target,
        &transport,
        clipboard::TransferPlan {
            media: PNG,
            staging: clipboard::Staging::Resume {
                path: &path,
                staged: offset,
            },
            length: whole.len() as u64,
        },
        &whole[offset as usize..],
    )
    .await?;
    let clipboard::Transferred::Complete(finished) = resumed else {
        anyhow::bail!("the resumed attempt did not finish the transfer");
    };
    println!(
        "resumed: {} bytes -> {}, digest matches source: {}",
        finished.bytes,
        finished.path,
        finished.digest == sha256(&whole)
    );

    clipboard::discard_upload(&target, &transport, &finished.path).await;
    let after = remote(&destination, &transport, staged_count)?;
    println!("  staged directories before {before}, after {after} (equal means nothing was left)");

    match (std::env::args().nth(2), std::env::args().nth(3)) {
        (Some(session), Some(socket)) => {
            relay(&destination, &transport, &session, &socket).await?;
        }
        _ => println!(
            "relay: skipped — pass a session and socket to exercise it, since an upload needs a \
             pane's control lease and a lease needs a route that opens"
        ),
    }

    for path in [uploaded.path, sent.path] {
        let _ = remote(
            &destination,
            &transport,
            &format!("rm -rf -- $(dirname {path})"),
        );
    }
    Ok(())
}

/// Drive the daemon's protocol against a target that really is remote.
///
/// A payload larger than one protocol message, so reassembly is exercised
/// rather than assumed, followed by a transfer abandoned mid-stream to check
/// the host is left as clean as a refusal leaves it.
async fn relay(
    destination: &str,
    transport: &TransportConfig,
    session: &str,
    socket: &str,
) -> Result<()> {
    use super_herdr::daemon::server::{DaemonOptions, spawn_in_process};
    use super_herdr::protocol::{ClientMessage, PROTOCOL_VERSION, ServerMessage, decode, encode};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let directory = tempfile::tempdir()?;
    let daemon = spawn_in_process(
        super_herdr::config::Config {
            transport: transport.clone(),
            notifications: Default::default(),
            transfers: Default::default(),
            web: Default::default(),
            targets: vec![Target {
                name: "qualify".to_owned(),
                ssh: Some(destination.to_owned()),
                discover_sessions: false,
                session: Some(session.to_owned()),
                socket: Some(socket.to_owned()),
                herdr_bins: vec!["/home/veba/.local/bin/herdr".to_owned(), "herdr".to_owned()],
            }],
            devices: Vec::new(),
        },
        None,
        DaemonOptions {
            socket: directory.path().join("unused.sock"),
            attention_state: Some(directory.path().join("attention.json")),
            refresh_interval: std::time::Duration::from_secs(3600),
            web_port: None,
            web_address: None,
            web_url: None,
        },
    );

    let (reader, mut writer) = tokio::io::split(daemon.attach()?);
    let mut reader = BufReader::new(reader);
    let send = |message: ClientMessage| -> Result<Vec<u8>> { encode(&message) };
    writer
        .write_all(&send(ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            client: "qualify".to_owned(),
        })?)
        .await?;

    // A pane that actually exists on the far side, taken from the daemon's own
    // view of it. Guessing an identifier would test nothing but the guess.
    writer
        .write_all(&send(ClientMessage::SubscribeState)?)
        .await?;
    let mut line = Vec::new();
    let pane = loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).await? == 0 {
            anyhow::bail!("the daemon closed before reporting any state");
        }
        line.pop();
        // State arrives once whole and then as per-target deltas, so both carry
        // the first snapshot depending on how quickly the target connects.
        let snapshot = match decode::<ServerMessage>(&line) {
            Ok(ServerMessage::FederationState { state }) => state
                .targets
                .values()
                .filter_map(|target| target.snapshot.clone())
                .next(),
            Ok(ServerMessage::TargetState {
                state: Some(target),
                ..
            }) => target.snapshot.clone(),
            _ => None,
        };
        if let Some(pane) = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.panes.keys().next())
        {
            break pane.clone();
        }
    };
    println!("relay: using pane {pane}");

    // The lease an upload needs. This one holds, because the route behind it
    // opens against a real Herdr server.
    writer
        .write_all(&send(ClientMessage::SubscribePane {
            pane: pane.clone(),
            access: super_herdr::terminal::TerminalAccess::Control,
            cols: 80,
            rows: 24,
            // This harness drives an upload, not a viewer; it wants the lease,
            // not a rendered screen.
            representation: super_herdr::protocol::PaneRepresentation::Frames,
        })?)
        .await?;

    // Larger than one protocol message, so chunking is real.
    let payload = png(6 * 1024 * 1024);
    writer
        .write_all(&send(ClientMessage::BeginUpload {
            request: 1,
            pane: pane.clone(),
            mime: "image/png".to_owned(),
            // A name the caller chose, so the qualification covers the path a
            // file takes rather than only the one a screenshot takes.
            name: Some("qualification-6mib.png".to_owned()),
            length: payload.len() as u64,
        })?)
        .await?;
    for chunk in payload.chunks(512 * 1024) {
        writer
            .write_all(&send(ClientMessage::UploadChunk {
                request: 1,
                bytes: chunk.to_vec(),
            })?)
            .await?;
    }
    writer
        .write_all(&send(ClientMessage::FinishUpload {
            request: 1,
            digest: sha256(&payload),
        })?)
        .await?;

    let staged = loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).await? == 0 {
            anyhow::bail!("the daemon closed before answering the relay");
        }
        line.pop();
        match decode::<ServerMessage>(&line) {
            Ok(ServerMessage::UploadComplete { path, bytes, .. }) => break (path, bytes),
            Ok(ServerMessage::Error {
                request: Some(1),
                message,
            }) => {
                anyhow::bail!("the relay refused a payload it should have carried: {message}");
            }
            _ => continue,
        }
    };
    let stored = remote(
        destination,
        transport,
        &format!("sha256sum {} | cut -d' ' -f1", staged.0),
    )?;
    println!("relay: {} bytes -> {}", staged.1, staged.0);
    println!(
        "  remote digest matches source: {}",
        stored == sha256(&payload)
    );

    // Abandoned mid-stream: nothing was ever sent onward, so the host should
    // look exactly as it did.
    let before = remote(destination, transport, STAGED_COUNT)?;
    writer
        .write_all(&send(ClientMessage::BeginUpload {
            request: 2,
            pane,
            mime: "image/png".to_owned(),
            name: None,
            length: 4096,
        })?)
        .await?;
    writer
        .write_all(&send(ClientMessage::UploadChunk {
            request: 2,
            bytes: png(1024),
        })?)
        .await?;
    drop(writer);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let after = remote(destination, transport, STAGED_COUNT)?;
    println!(
        "abandoned mid-stream: staged before {before}, after {after} (equal means nothing was left)"
    );

    let _ = remote(
        destination,
        transport,
        &format!("rm -rf -- $(dirname {})", staged.0),
    );
    Ok(())
}

const STAGED_COUNT: &str =
    "ls -d ${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/super-herdr-clipboard.* 2>/dev/null | wc -l";

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
