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

/// How long a paired device's cookie lasts before the browser drops it. The
/// token itself does not expire — revoking is what ends it — but a browser that
/// has not been used in a month should ask again rather than hold a credential
/// indefinitely.
const COOKIE_LIFETIME_SECONDS: u64 = 60 * 60 * 24 * 30;

pub fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Whether the daemon will bind here.
///
/// A token authenticates a device; it does not encrypt anything. On a private
/// mesh the network already provides confidentiality, and on the open internet
/// it would not — so a public address is refused rather than warned about, and
/// the way to reach a daemon from outside remains a forwarded port, which is an
/// explicit act rather than a flag someone set once.
pub fn bindable(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                // 100.64.0.0/10, the shared range Tailscale and other meshes use.
                || (address.octets()[0] == 100 && (64..128).contains(&address.octets()[1]))
        }
        IpAddr::V6(address) => {
            address.is_loopback()
                // Unique local (fc00::/7) and link local (fe80::/10).
                || (address.segments()[0] & 0xfe00) == 0xfc00
                || (address.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Opens one in-process attachment to the daemon.
pub type Attach = Arc<dyn Fn() -> Result<DuplexStream> + Send + Sync>;

/// What the web layer asks of pairing, so it holds no policy of its own.
pub trait Devices: Send + Sync {
    /// Whether this token belongs to a paired device.
    fn admits(&self, token: &str) -> bool;
    /// Exchange a pairing code for a new device's token, or refuse.
    fn pair(&self, code: &str, name: &str) -> Result<String>;
    /// The daemon's own version, shown to a person so a stale daemon is
    /// visible where it would otherwise be invisible.
    fn version(&self) -> String;
}

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
    anyhow::ensure!(
        bindable(address.ip()),
        "refusing to serve the web client on {address}: a device token authenticates but does not \
         encrypt, so this is offered only on loopback or a private network. Forward the port \
         instead."
    );
    TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to serve the web client on {address}"))
}

pub async fn serve(listener: TcpListener, attach: Attach, devices: Arc<dyn Devices>) {
    let sessions = Arc::new(Sessions::default());
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let attach = attach.clone();
                let sessions = sessions.clone();
                let devices = devices.clone();
                tokio::spawn(async move {
                    let _ = handle(stream, peer, attach, sessions, devices).await;
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
    token: Option<String>,
    length: usize,
}

async fn handle(
    stream: TcpStream,
    peer: SocketAddr,
    attach: Attach,
    sessions: Arc<Sessions>,
    devices: Arc<dyn Devices>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let Some(request) = read_request(&mut reader).await? else {
        return Ok(());
    };

    // Loopback is not asked for a token. Anyone who can reach it can already
    // read the daemon's socket, so requiring one would be ceremony rather than
    // a boundary. Everything arriving over a network must present one.
    let admitted = peer.ip().is_loopback()
        || request
            .token
            .as_deref()
            .is_some_and(|token| devices.admits(token));

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => write_page(&mut writer, APP).await,
        ("GET", "/session") => {
            // What the page needs before it can decide what to show: whether
            // this browser is already paired, and which daemon it is talking
            // to.
            let body = format!(
                "{{\"paired\":{admitted},\"version\":\"{}\"}}",
                devices.version().replace('"', "")
            );
            write_json(&mut writer, &body).await
        }
        ("POST", "/pair") => {
            let body = read_body(&mut reader, request.length).await?;
            let text = String::from_utf8_lossy(&body);
            let code = json_field(&text, "code").unwrap_or_default();
            let name = json_field(&text, "name").unwrap_or_default();
            match devices.pair(&code, &name) {
                Ok(token) => {
                    let cookie = format!(
                        "sh_device={token}; Path=/; Max-Age={COOKIE_LIFETIME_SECONDS}; HttpOnly; SameSite=Strict"
                    );
                    write_with_cookie(&mut writer, "204 No Content", &cookie).await
                }
                // The reason is the daemon's, and says which check failed
                // without saying anything about codes that would have worked.
                Err(error) => write_status(&mut writer, "403 Forbidden", &error.to_string()).await,
            }
        }
        _ if !admitted => {
            write_status(
                &mut writer,
                "401 Unauthorized",
                "this device is not paired with this daemon",
            )
            .await
        }
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
    let mut token = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse::<usize>().unwrap_or_default();
        }
        if name.eq_ignore_ascii_case("cookie") {
            token = value
                .split(';')
                .filter_map(|pair| pair.trim().strip_prefix("sh_device="))
                .map(sanitized_token)
                .find(|token| !token.is_empty());
        }
    }

    Ok(Some(Request {
        method,
        path: path.to_owned(),
        session,
        token,
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

/// A token is hex from this daemon, so anything else is not one and is dropped
/// rather than compared.
fn sanitized_token(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(128)
        .collect()
}

/// Read one string field out of a small JSON object.
///
/// The pairing request is two short fields from a page this daemon served, so
/// it is read directly rather than through a model that would have to be kept
/// in step with the page.
fn json_field(text: &str, field: &str) -> Option<String> {
    let key = format!("\"{field}\"");
    let start = text.find(&key)? + key.len();
    let rest = text
        .get(start..)?
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest.get(..end)?.to_owned())
}

async fn write_json(writer: &mut OwnedWriteHalf, body: &str) -> Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
        body.len()
    );
    writer.write_all(head.as_bytes()).await?;
    writer.write_all(body.as_bytes()).await?;
    let _ = writer.shutdown().await;
    Ok(())
}

async fn write_with_cookie(writer: &mut OwnedWriteHalf, status: &str, cookie: &str) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nset-cookie: {cookie}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    writer.write_all(head.as_bytes()).await?;
    let _ = writer.shutdown().await;
    Ok(())
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
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    use super::{APP, DEFAULT_WEB_PORT, loopback, sanitized_session};
    use crate::config::Config;
    use crate::daemon::server::{DaemonOptions, spawn_in_process};

    /// Stands in for the daemon's pairing policy. Loopback tests never consult
    /// it, which is itself the property the network tests check.
    #[derive(Default)]
    struct TestDevices {
        admitted: Mutex<Vec<String>>,
        code: Mutex<Option<String>>,
    }

    impl super::Devices for TestDevices {
        fn admits(&self, token: &str) -> bool {
            self.admitted
                .lock()
                .map(|held| held.iter().any(|held| held == token))
                .unwrap_or(false)
        }

        fn pair(&self, code: &str, _name: &str) -> anyhow::Result<String> {
            let waiting = self.code.lock().ok().and_then(|mut held| held.take());
            match waiting {
                Some(waiting) if waiting == code => {
                    let token = "ab".repeat(32);
                    if let Ok(mut admitted) = self.admitted.lock() {
                        admitted.push(token.clone());
                    }
                    Ok(token)
                }
                Some(_) => anyhow::bail!("that pairing code is not the one waiting"),
                None => anyhow::bail!("no pairing code is waiting"),
            }
        }

        fn version(&self) -> String {
            "0.0.0-test".to_owned()
        }
    }

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

    async fn serve_test_daemon() -> (
        u16,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
        Arc<TestDevices>,
    ) {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let daemon = spawn_in_process(
            Config {
                transport: Default::default(),
                notifications: Default::default(),
                targets: Vec::new(),
                devices: Vec::new(),
            },
            None,
            DaemonOptions {
                socket: directory.path().join("unused.sock"),
                attention_state: Some(directory.path().join("attention.json")),
                refresh_interval: Duration::from_secs(3600),
                web_port: None,
                web_address: None,
            },
        );
        // Port zero so tests never collide with a daemon somebody is running.
        let listener = super::bind(loopback(0)).await.expect("binds loopback");
        let port = listener.local_addr().expect("a bound address").port();
        let daemon = std::sync::Arc::new(daemon);
        let attach: super::Attach = std::sync::Arc::new(move || daemon.attach());
        let devices = Arc::new(TestDevices::default());
        let task = tokio::spawn(super::serve(listener, attach, devices.clone()));
        // The directory is returned rather than leaked, so a test run does not
        // accumulate state directories.
        (port, task, directory, devices)
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
        let (port, task, _directory, _devices) = serve_test_daemon().await;

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
        let (port, task, _directory, _devices) = serve_test_daemon().await;

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
        let (port, task, _directory, _devices) = serve_test_daemon().await;
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
    fn a_public_address_is_refused_and_a_private_one_is_not() {
        use std::net::IpAddr;

        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.4",
            // The shared range meshes such as Tailscale hand out.
            "100.87.78.39",
            "::1",
            "fd00::1",
        ] {
            let address: IpAddr = private.parse().unwrap();
            assert!(super::bindable(address), "{private} should be bindable");
        }
        for public in [
            "8.8.8.8",
            "203.0.113.7",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700::1111",
        ] {
            let address: IpAddr = public.parse().unwrap();
            assert!(
                !super::bindable(address),
                "{public} must not be bindable: a token authenticates but does not encrypt"
            );
        }
        // Nor the wildcard, which would include every public interface.
        assert!(!super::bindable("0.0.0.0".parse().unwrap()));
    }

    #[tokio::test]
    async fn pairing_exchanges_a_code_for_a_cookie_once() {
        let (port, task, _directory, devices) = serve_test_daemon().await;
        if let Ok(mut code) = devices.code.lock() {
            *code = Some("ABCD2345".to_owned());
        }
        let body = r#"{"code":"ABCD2345","name":"phone"}"#;
        let pair = format!(
            "POST /pair HTTP/1.1\r\nhost: localhost\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );

        let response = request(port, &pair).await;
        assert!(response.contains("204"), "{response}");

        // A code is spent by the match, so the same one does not pair a second
        // device.
        let response = request(port, &pair).await;
        assert!(response.contains("403"), "{response}");
        task.abort();
    }

    /// Cookie attributes are split off before a value reaches the filter, so
    /// what it sees is one value. The filter's job is only to refuse anything
    /// that is not shaped like a digest this daemon issued.
    #[test]
    fn a_token_value_is_hex_or_nothing() {
        assert_eq!(super::sanitized_token("00ff"), "00ff");
        assert_eq!(super::sanitized_token(" 00ff "), "00ff");
        assert_eq!(super::sanitized_token("zz!!"), "");
        assert_eq!(super::sanitized_token(&"a".repeat(500)).len(), 128);

        // The invariant rather than a hand-computed result: whatever arrives,
        // what comes out is hex and bounded, so nothing structural survives to
        // be compared against a stored digest.
        for hostile in [
            "../../etc/passwd",
            "00ff\r\nx: y",
            "'; DROP--",
            "\u{0}\u{7f}",
        ] {
            let cleaned = super::sanitized_token(hostile);
            assert!(
                cleaned
                    .chars()
                    .all(|character| character.is_ascii_hexdigit()),
                "{hostile:?} produced {cleaned:?}"
            );
            assert!(cleaned.len() <= 128);
        }
    }

    #[test]
    fn one_json_field_is_read_without_a_parser() {
        let text = r#"{"code":"ABCD-2345","name":"my phone"}"#;
        assert_eq!(super::json_field(text, "code").unwrap(), "ABCD-2345");
        assert_eq!(super::json_field(text, "name").unwrap(), "my phone");
        assert!(super::json_field(text, "absent").is_none());
        assert!(super::json_field("not json at all", "code").is_none());
    }

    #[test]
    fn the_default_port_is_loopback_only() {
        assert!(loopback(DEFAULT_WEB_PORT).ip().is_loopback());
    }
}
