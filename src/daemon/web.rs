//! A browser client, served by the daemon over loopback HTTP.
//!
//! A browser cannot open a Unix socket, so the same protocol is carried over
//! two ordinary HTTP requests instead of a new one: the daemon's messages
//! arrive on a server-sent event stream, and a client's messages are posted to
//! it. That needs no framing, no masking, and no handshake beyond HTTP itself,
//! which is why there is a hand-written server here rather than a web stack.
//! When the client needs to type into a pane, the latency of a post per
//! keystroke will argue for a socket upgrade; while it only observes, it does
//! not.
//!
//! What sits behind both requests is one ordinary in-process attachment, so a
//! browser is a client of the same daemon in the same way the frontend is,
//! speaking the same framing through the same handshake. Nothing here
//! interprets a protocol message; the browser is handed the vocabulary every
//! other client receives.
//!
//! The listener binds loopback and nothing else. There is no authentication yet
//! — that is device pairing, and it is deliberately a separate decision — so a
//! device reaches this the way it reaches any other loopback service on another
//! machine: forwarded over OpenSSH. Binding elsewhere is not offered rather
//! than discouraged, because a flag that publishes an unauthenticated
//! federation is the kind of thing that gets used once and regretted.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::protocol::{ClientMessage, MAX_MESSAGE_BYTES, PROTOCOL_VERSION, encode};

/// The page and everything it needs. Held in the binary so a daemon serves a
/// client without an install step, a file path, or anything to keep in sync.
const APP: &str = include_str!("app.html");

/// A request line and its headers may not exceed this. A browser sends a few
/// hundred bytes; anything approaching this is not one.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// A session identifier from a browser is untrusted text used only as a map
/// key, and is bounded and restricted to characters that cannot confuse a log
/// or a header.
const MAX_SESSION_CHARS: usize = 64;

pub const DEFAULT_WEB_PORT: u16 = 8790;

pub fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Opens one in-process attachment to the daemon.
pub type Attach = Arc<dyn Fn() -> Result<DuplexStream> + Send + Sync>;

/// Browser sessions, each holding the writing half of its own attachment.
///
/// A stream and the posts that steer it are separate requests, so they are
/// joined here by an identifier the browser generates. Without that, a
/// subscription posted by one request would be made on a connection some other
/// request is reading, which is the kind of thing that works in testing and
/// fails the moment a second tab is open.
#[derive(Default)]
struct Sessions {
    open: Mutex<BTreeMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
}

pub async fn bind(address: SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to serve the web client on {address}"))
}

pub async fn serve(listener: TcpListener, attach: Attach) {
    let sessions = Arc::new(Sessions::default());
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let attach = attach.clone();
                let sessions = sessions.clone();
                tokio::spawn(async move {
                    let _ = handle(stream, attach, sessions).await;
                });
            }
            // One failed accept must not end the server.
            Err(_) => continue,
        }
    }
}

struct Request {
    method: String,
    path: String,
    session: Option<String>,
    length: usize,
}

async fn handle(stream: TcpStream, attach: Attach, sessions: Arc<Sessions>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let Some(request) = read_request(&mut reader).await? else {
        return Ok(());
    };

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => write_page(&mut writer, APP).await,
        ("GET", "/events") => {
            let Some(session) = request.session else {
                return write_status(&mut writer, "400 Bad Request", "a session is required").await;
            };
            match attach() {
                Ok(daemon) => stream_events(writer, daemon, session, sessions).await,
                Err(_) => {
                    write_status(&mut writer, "503 Service Unavailable", "daemon unavailable").await
                }
            }
        }
        ("POST", "/command") => {
            let Some(session) = request.session else {
                return write_status(&mut writer, "400 Bad Request", "a session is required").await;
            };
            let body = read_body(&mut reader, request.length).await?;
            let sender = sessions
                .open
                .lock()
                .ok()
                .and_then(|open| open.get(&session).cloned());
            let Some(sender) = sender else {
                return write_status(&mut writer, "409 Conflict", "no open event stream").await;
            };
            // Parsed before forwarding, so a browser cannot post something the
            // protocol would refuse and learn about it only as a dropped
            // connection.
            let forwarded = crate::protocol::decode::<ClientMessage>(body.trim_ascii())
                .ok()
                .and_then(|message| encode(&message).ok())
                .is_some_and(|line| sender.send(line).is_ok());
            if forwarded {
                write_status(&mut writer, "204 No Content", "").await
            } else {
                write_status(&mut writer, "400 Bad Request", "unreadable command").await
            }
        }
        _ => write_status(&mut writer, "404 Not Found", "no such path").await,
    }
}

async fn read_request(reader: &mut BufReader<OwnedReadHalf>) -> Result<Option<Request>> {
    let mut head = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let remaining = MAX_HEADER_BYTES.saturating_sub(head.len());
        if remaining == 0 {
            return Ok(None);
        }
        let read = reader
            .take(remaining as u64)
            .read_until(b'\n', &mut line)
            .await?;
        if read == 0 {
            return Ok(None);
        }
        let blank = line == b"\r\n" || line == b"\n";
        head.extend_from_slice(&line);
        if blank {
            break;
        }
    }

    let text = String::from_utf8_lossy(&head);
    let mut lines = text.lines();
    let mut start = lines.next().unwrap_or_default().split_whitespace();
    let method = start.next().unwrap_or_default().to_owned();
    let target = start.next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let session = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("session="))
        .map(sanitized_session)
        .filter(|session| !session.is_empty());

    let mut length = 0;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().unwrap_or_default();
        }
    }

    Ok(Some(Request {
        method,
        path: path.to_owned(),
        session,
        length: length.min(MAX_MESSAGE_BYTES),
    }))
}

/// A session identifier is only ever a map key, so it keeps the characters that
/// can be one and nothing else.
fn sanitized_session(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(MAX_SESSION_CHARS)
        .collect()
}

async fn read_body(reader: &mut BufReader<OwnedReadHalf>, length: usize) -> Result<Vec<u8>> {
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

async fn write_page(writer: &mut OwnedWriteHalf, body: &str) -> Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
        body.len()
    );
    writer.write_all(head.as_bytes()).await?;
    writer.write_all(body.as_bytes()).await?;
    let _ = writer.shutdown().await;
    Ok(())
}

async fn write_status(writer: &mut OwnedWriteHalf, status: &str, body: &str) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    writer.write_all(head.as_bytes()).await?;
    writer.write_all(body.as_bytes()).await?;
    let _ = writer.shutdown().await;
    Ok(())
}

/// Carry one attachment's messages as server-sent events.
///
/// Each event is one protocol line, exactly as it would appear on the socket,
/// so the browser is given the same vocabulary as every other client rather
/// than a translation of it.
async fn stream_events(
    mut writer: OwnedWriteHalf,
    daemon: DuplexStream,
    session: String,
    sessions: Arc<Sessions>,
) -> Result<()> {
    let (daemon_reader, mut daemon_writer) = tokio::io::split(daemon);
    let hello = encode(&ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        client: "super-herdr-web".to_owned(),
    })?;
    daemon_writer.write_all(&hello).await?;

    let (commands, mut posted) = mpsc::unbounded_channel::<Vec<u8>>();
    if let Ok(mut open) = sessions.open.lock() {
        open.insert(session.clone(), commands);
    }
    let writing = tokio::spawn(async move {
        while let Some(line) = posted.recv().await {
            if daemon_writer.write_all(&line).await.is_err() {
                break;
            }
        }
    });

    writer
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
        )
        .await?;

    let mut daemon_reader = BufReader::new(daemon_reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut daemon_reader)
            .take(MAX_MESSAGE_BYTES as u64)
            .read_until(b'\n', &mut line)
            .await;
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) if line.last() != Some(&b'\n') => break,
            Ok(_) => {}
        }
        // A protocol line is one JSON object and carries no embedded newline,
        // so it needs no escaping to become one event.
        if writer.write_all(b"data: ").await.is_err()
            || writer.write_all(&line).await.is_err()
            || writer.write_all(b"\n").await.is_err()
        {
            break;
        }
    }

    if let Ok(mut open) = sessions.open.lock() {
        open.remove(&session);
    }
    writing.abort();
    let _ = writer.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    use super::{APP, DEFAULT_WEB_PORT, loopback, sanitized_session};
    use crate::config::Config;
    use crate::daemon::server::{DaemonOptions, spawn_in_process};

    #[test]
    fn a_session_identifier_is_only_ever_a_map_key() {
        assert_eq!(sanitized_session("abc-123"), "abc-123");
        // Anything that could confuse a header, a log, or a path is dropped
        // rather than escaped, because nothing needs those characters.
        assert_eq!(
            sanitized_session("a/../b\r\nx: y"),
            "abx y".replace(' ', "")
        );
        assert_eq!(sanitized_session(&"x".repeat(500)).len(), 64);
        assert!(sanitized_session("../../etc/passwd").starts_with("etcpasswd"));
    }

    /// The page must work with no network beyond the daemon itself: a phone on
    /// a forwarded port has no route to anywhere else, and a client that
    /// silently needed one would fail exactly where it matters.
    #[test]
    fn the_page_reaches_for_nothing_it_is_not_served() {
        for marker in ["http://", "https://", "//cdn", "integrity="] {
            assert!(
                !APP.contains(marker),
                "the page refers to {marker}, which a forwarded port cannot reach"
            );
        }
        assert!(
            APP.contains("EventSource"),
            "the page opens the event stream"
        );
    }

    async fn serve_test_daemon() -> (u16, tokio::task::JoinHandle<()>, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let daemon = spawn_in_process(
            Config {
                transport: Default::default(),
                notifications: Default::default(),
                targets: Vec::new(),
            },
            None,
            DaemonOptions {
                socket: directory.path().join("unused.sock"),
                attention_state: Some(directory.path().join("attention.json")),
                refresh_interval: Duration::from_secs(3600),
                web_port: None,
            },
        );
        // Port zero so tests never collide with a daemon somebody is running.
        let listener = super::bind(loopback(0)).await.expect("binds loopback");
        let port = listener.local_addr().expect("a bound address").port();
        let daemon = std::sync::Arc::new(daemon);
        let attach: super::Attach = std::sync::Arc::new(move || daemon.attach());
        let task = tokio::spawn(super::serve(listener, attach));
        // The directory is returned rather than leaked, so a test run does not
        // accumulate state directories.
        (port, task, directory)
    }

    async fn request(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(loopback(port))
            .await
            .expect("the web client accepts a connection");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("the request is written");
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader
            .read_line(&mut status)
            .await
            .expect("a status line comes back");
        status
    }

    #[tokio::test]
    async fn the_page_is_served_and_unknown_paths_are_not() {
        let (port, task, _directory) = serve_test_daemon().await;

        assert!(
            request(port, "GET / HTTP/1.1\r\nhost: localhost\r\n\r\n")
                .await
                .contains("200")
        );
        assert!(
            request(port, "GET /nothing HTTP/1.1\r\nhost: localhost\r\n\r\n")
                .await
                .contains("404")
        );
        task.abort();
    }

    #[tokio::test]
    async fn a_command_without_its_stream_is_refused() {
        let (port, task, _directory) = serve_test_daemon().await;

        // No event stream has claimed this session, so there is no attachment
        // to steer and the post is refused rather than opening one.
        let body = r#"{"type":"state.subscribe"}"#;
        let status = request(
            port,
            &format!(
                "POST /command?session=orphan HTTP/1.1\r\nhost: localhost\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
        .await;
        assert!(status.contains("409"), "{status}");

        // And a stream without a session is refused too, since nothing could
        // ever steer it.
        let status = request(port, "GET /events HTTP/1.1\r\nhost: localhost\r\n\r\n").await;
        assert!(status.contains("400"), "{status}");
        task.abort();
    }

    #[tokio::test]
    async fn a_stream_carries_the_handshake_the_socket_carries() {
        let (port, task, _directory) = serve_test_daemon().await;
        let mut stream = TcpStream::connect(loopback(port)).await.expect("connects");
        stream
            .write_all(b"GET /events?session=one HTTP/1.1\r\nhost: localhost\r\n\r\n")
            .await
            .expect("writes");

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let greeting = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                line.clear();
                reader.read_line(&mut line).await.expect("reads");
                if line.starts_with("data: ") {
                    return line.clone();
                }
            }
        })
        .await
        .expect("the daemon greets a browser as it greets anything else");

        // The browser is handed protocol messages, not a translation of them.
        assert!(greeting.contains("server.hello"), "{greeting}");
        assert!(greeting.contains("\"protocol\":1"), "{greeting}");
        task.abort();
    }

    #[test]
    fn the_default_port_is_loopback_only() {
        assert!(loopback(DEFAULT_WEB_PORT).ip().is_loopback());
    }
}
