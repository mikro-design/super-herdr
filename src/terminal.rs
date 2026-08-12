use std::process::Stdio;

use anyhow::{Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::config::{Target, TransportConfig};
use crate::model::PaneId;
use crate::transport::build_herdr_command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalAccess {
    Observe,
    Control,
}

pub struct TerminalProcess {
    pub child: Child,
    pub input: Option<ChildStdin>,
    pub output: ChildStdout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    Frame {
        sequence: u64,
        width: u16,
        height: u16,
        full: bool,
        bytes: Vec<u8>,
    },
    Closed,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum TerminalEnvelope {
    #[serde(rename = "terminal.frame")]
    Frame {
        seq: u64,
        encoding: String,
        width: u16,
        height: u16,
        full: bool,
        bytes: String,
    },
    #[serde(rename = "terminal.closed")]
    Closed { reason: Option<String> },
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum TerminalCommand<'a> {
    #[serde(rename = "terminal.input")]
    Input { bytes: &'a str },
    #[serde(rename = "terminal.resize")]
    Resize {
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    },
    #[serde(rename = "terminal.scroll")]
    Scroll {
        direction: TerminalScrollDirection,
        lines: u16,
        source: TerminalScrollSource,
        column: u16,
        row: u16,
        modifiers: u8,
    },
    #[serde(rename = "terminal.release")]
    Release {},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalScrollDirection {
    Up,
    Down,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum TerminalScrollSource {
    Wheel,
}

pub fn parse_terminal_event(line: &[u8]) -> Result<TerminalEvent> {
    let envelope = serde_json::from_slice::<TerminalEnvelope>(line)
        .context("invalid terminal session envelope")?;
    match envelope {
        TerminalEnvelope::Frame {
            seq,
            encoding,
            width,
            height,
            full,
            bytes,
        } => {
            anyhow::ensure!(encoding == "ansi", "unsupported terminal frame encoding");
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(bytes)
                .context("invalid terminal frame payload")?;
            Ok(TerminalEvent::Frame {
                sequence: seq,
                width,
                height,
                full,
                bytes,
            })
        }
        TerminalEnvelope::Closed { reason } => {
            let _ = reason;
            Ok(TerminalEvent::Closed)
        }
    }
}

pub fn terminal_input_command(bytes: &[u8]) -> Result<Vec<u8>> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    encode_command(&TerminalCommand::Input { bytes: &encoded })
}

pub fn terminal_resize_command(columns: u16, rows: u16) -> Result<Vec<u8>> {
    encode_command(&TerminalCommand::Resize {
        cols: columns.max(1),
        rows: rows.max(1),
        cell_width_px: 0,
        cell_height_px: 0,
    })
}

pub fn terminal_scroll_command(
    direction: TerminalScrollDirection,
    lines: u16,
    column: u16,
    row: u16,
    modifiers: u8,
) -> Result<Vec<u8>> {
    encode_command(&TerminalCommand::Scroll {
        direction,
        lines: lines.max(1),
        source: TerminalScrollSource::Wheel,
        column,
        row,
        modifiers,
    })
}

pub fn terminal_release_command() -> Result<Vec<u8>> {
    encode_command(&TerminalCommand::Release {})
}

fn encode_command(command: &TerminalCommand<'_>) -> Result<Vec<u8>> {
    let mut line = serde_json::to_vec(command).context("failed to encode terminal command")?;
    line.push(b'\n');
    Ok(line)
}

pub fn spawn_terminal(
    target: &Target,
    transport: &TransportConfig,
    executable: &str,
    pane: &PaneId,
    access: TerminalAccess,
    rows: u16,
    columns: u16,
) -> Result<TerminalProcess> {
    let args = terminal_operation_args(pane, access, rows, columns);
    let mut command = build_herdr_command(target, transport, executable, &args);
    command
        .stdin(if access == TerminalAccess::Control {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to open terminal route for {}", pane))?;
    let input = child.stdin.take();
    let output = child
        .stdout
        .take()
        .context("terminal route did not expose an output stream")?;
    Ok(TerminalProcess {
        child,
        input,
        output,
    })
}

fn terminal_operation_args(
    pane: &PaneId,
    access: TerminalAccess,
    rows: u16,
    columns: u16,
) -> Vec<String> {
    let access_command = match access {
        TerminalAccess::Observe => "observe",
        TerminalAccess::Control => "control",
    };
    vec![
        "terminal".to_owned(),
        "session".to_owned(),
        access_command.to_owned(),
        pane.server_local_id().to_owned(),
        "--cols".to_owned(),
        columns.max(1).to_string(),
        "--rows".to_owned(),
        rows.max(1).to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use serde_json::Value;

    use super::{
        TerminalEvent, TerminalScrollDirection, parse_terminal_event, terminal_input_command,
        terminal_operation_args, terminal_release_command, terminal_resize_command,
        terminal_scroll_command,
    };
    use crate::model::PaneId;

    #[test]
    fn decodes_public_terminal_frame_envelope() {
        let payload = base64::engine::general_purpose::STANDARD.encode(b"hello");
        let line = format!(
            r#"{{"type":"terminal.frame","seq":7,"encoding":"ansi","width":80,"height":24,"full":true,"bytes":"{payload}"}}"#
        );

        assert_eq!(
            parse_terminal_event(line.as_bytes()).unwrap(),
            TerminalEvent::Frame {
                sequence: 7,
                width: 80,
                height: 24,
                full: true,
                bytes: b"hello".to_vec(),
            }
        );
    }

    #[test]
    fn encodes_binary_input_as_base64_json_line() {
        let line = terminal_input_command(&[0, 2, 255]).unwrap();
        assert_eq!(line.last(), Some(&b'\n'));
        let value: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(value["type"], "terminal.input");
        assert_eq!(value["bytes"], "AAL/");
    }

    #[test]
    fn encodes_resize_and_release_commands() {
        let resize: Value =
            serde_json::from_slice(&terminal_resize_command(90, 30).unwrap()).unwrap();
        let release: Value = serde_json::from_slice(&terminal_release_command().unwrap()).unwrap();

        assert_eq!(resize["type"], "terminal.resize");
        assert_eq!(resize["cols"], 90);
        assert_eq!(resize["rows"], 30);
        assert_eq!(release["type"], "terminal.release");
    }

    #[test]
    fn encodes_documented_terminal_scroll_command() {
        let scroll: Value = serde_json::from_slice(
            &terminal_scroll_command(TerminalScrollDirection::Up, 3, 12, 5, 3).unwrap(),
        )
        .unwrap();

        assert_eq!(scroll["type"], "terminal.scroll");
        assert_eq!(scroll["direction"], "up");
        assert_eq!(scroll["lines"], 3);
        assert_eq!(scroll["source"], "wheel");
        assert_eq!(scroll["column"], 12);
        assert_eq!(scroll["row"], 5);
        assert_eq!(scroll["modifiers"], 3);
    }

    #[test]
    fn rejects_non_ansi_frames() {
        let line = br#"{"type":"terminal.frame","seq":1,"encoding":"cells","width":1,"height":1,"full":false,"bytes":""}"#;
        assert!(parse_terminal_event(line).is_err());
    }

    #[test]
    fn puts_terminal_target_before_options_as_required_by_herdr_cli() {
        let args = terminal_operation_args(
            &PaneId::new("ws01", "dev", "w1:p1"),
            super::TerminalAccess::Control,
            24,
            80,
        );

        assert_eq!(
            args,
            [
                "terminal", "session", "control", "w1:p1", "--cols", "80", "--rows", "24"
            ]
        );
    }
}
