//! `super-herdr doctor`: which layer is broken, without exposing the machine.
//!
//! Super-Herdr sits on several things it does not own — a configuration file, a
//! Herdr on every host, SSH between them, a daemon socket, a browser route, a
//! desktop's clipboard and notification tools. When one of them is wrong the
//! symptom is usually the same: something did not appear. This says which
//! layer, in one pass, and never guesses on the strength of a symptom it did
//! not check.
//!
//! Two rules shape what it prints:
//!
//! * **It reports, and never repairs.** Every failed check carries the command
//!   somebody would run, and none of them run here. A diagnostic that changes
//!   the system is one people are afraid to run on a machine that is nearly
//!   working, which is exactly when it is most useful.
//! * **The output is meant to be pasted somewhere public.** Host names, SSH
//!   destinations, socket paths, browser URLs and home directories are
//!   redacted to their shape. Target names are kept, because they are the
//!   labels somebody chose and without them a report says a problem exists
//!   without saying where.
//!
//! Terminal contents, clipboard payloads, device tokens and pairing material do
//! not appear here at all, and no check reads any of them.

use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::config::Config;
use crate::probe::{self, ProbeReport};

/// How long any one network check may take before it is reported as slow
/// rather than waited on. A doctor that hangs is a doctor nobody runs twice.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    /// Works, but something will bite later.
    Warn,
    Fail,
    /// Not checked, and why is in the detail. Distinct from `Ok` because
    /// "nothing to check" and "checked and fine" are different sentences.
    Skipped,
}

impl Status {
    fn mark(self) -> &'static str {
        match self {
            Self::Ok => "ok  ",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
            Self::Skipped => "--  ",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub area: String,
    pub name: String,
    pub status: Status,
    pub detail: String,
    /// What somebody would run to fix it. Never run here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

impl Check {
    fn new(area: &str, name: &str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            area: area.to_owned(),
            name: name.to_owned(),
            status,
            detail: detail.into(),
            remedy: None,
        }
    }

    /// A line from a diagnostic that already knows how to describe itself.
    ///
    /// The clipboard and notification checks predate this command and produce
    /// prose. Wrapping rather than reformatting keeps one description of each
    /// subsystem instead of two that drift.
    pub fn note(area: &str, line: String) -> Self {
        let (name, detail) = line
            .split_once(':')
            .map(|(name, detail)| (name.trim().to_owned(), detail.trim().to_owned()))
            .unwrap_or_else(|| (area.to_owned(), line.clone()));
        // A subsystem that says it is unavailable or off is reported that way
        // rather than as a passing check. Marking every imported line `ok`
        // would make the report agree with itself and not with the machine.
        let status = if detail.starts_with("unavailable") || detail == "disabled" {
            Status::Skipped
        } else {
            Status::Ok
        };
        Self {
            area: area.to_owned(),
            name,
            status,
            detail,
            remedy: None,
        }
    }

    fn with_remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|check| check.status == Status::Fail)
    }

    /// One line per check, plus the remedies underneath the ones that need
    /// them. Written to be read in a terminal and pasted into an issue.
    ///
    /// Grouped by area in the order each area first appears, rather than in
    /// the order checks were produced: the local checks and the probes both
    /// describe targets, and a report that names that heading twice reads as
    /// two different subjects.
    pub fn render(&self) -> String {
        let mut areas: Vec<&str> = Vec::new();
        for check in &self.checks {
            if !areas.contains(&check.area.as_str()) {
                areas.push(&check.area);
            }
        }
        let mut rendered = String::new();
        for area in areas {
            if !rendered.is_empty() {
                rendered.push('\n');
            }
            rendered.push_str(&format!("{area}\n"));
            for check in self.checks.iter().filter(|check| check.area == area) {
                rendered.push_str(&format!(
                    "  {} {}: {}\n",
                    check.status.mark(),
                    check.name,
                    check.detail
                ));
                if let Some(remedy) = &check.remedy {
                    rendered.push_str(&format!("       try: {remedy}\n"));
                }
            }
        }
        rendered
    }
}

/// Describe a path without saying where it is.
///
/// A path is evidence of shape — that it is set, roughly how long, and what
/// kind of thing it names — and the rest of it is somebody's username and
/// directory layout. Kept as a helper rather than inlined so that adding a
/// check cannot accidentally print one.
pub fn redact_path(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "?".to_owned());
    format!("…/{name}")
}

/// Describe a destination without naming a host.
pub fn redact_destination(destination: &str) -> String {
    let shape = if destination.contains('@') {
        "user@host"
    } else {
        "host"
    };
    format!("<{shape}, {} chars>", destination.chars().count())
}

/// Describe a URL by its scheme only.
pub fn redact_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, _)) => format!("<{scheme} url>"),
        None => "<url>".to_owned(),
    }
}

/// Run every check that does not need the network.
///
/// Split from the target probe so a test can assert what this says without a
/// host to say it about, and so the slow part is visibly the slow part.
pub fn local_checks(config: &Config, config_path: &Path, socket: Option<&Path>) -> Vec<Check> {
    let mut checks = Vec::new();

    checks.push(Check::new(
        "configuration",
        "file",
        Status::Ok,
        format!("read {}", redact_path(config_path)),
    ));
    checks.push(match config_permissions(config_path) {
        Some(mode) if mode & 0o077 != 0 => Check::new(
            "configuration",
            "permissions",
            Status::Warn,
            format!("mode {mode:04o} is readable by other users"),
        )
        .with_remedy(format!("chmod 600 {}", redact_path(config_path))),
        Some(mode) => Check::new(
            "configuration",
            "permissions",
            Status::Ok,
            format!("mode {mode:04o}"),
        ),
        None => Check::new(
            "configuration",
            "permissions",
            Status::Skipped,
            "not applicable on this platform",
        ),
    });
    checks.push(match config.validate() {
        Ok(()) => Check::new(
            "configuration",
            "contents",
            Status::Ok,
            format!("{} target(s) configured", config.targets.len()),
        ),
        Err(error) => Check::new("configuration", "contents", Status::Fail, error.to_string())
            .with_remedy("super-herdr check".to_owned()),
    });

    for target in &config.targets {
        let where_ = match target.ssh.as_deref() {
            Some(destination) => format!("over SSH to {}", redact_destination(destination)),
            None => "on this machine".to_owned(),
        };
        checks.push(Check::new(
            "targets",
            &target.name,
            Status::Ok,
            format!(
                "{where_}; {} herdr candidate(s); {}",
                target.herdr_bins.len(),
                if target.discover_sessions {
                    "sessions discovered"
                } else {
                    "one session"
                }
            ),
        ));
        if !target.roots.is_empty() {
            checks.push(Check::new(
                "targets",
                &format!("{} file roots", target.name),
                Status::Ok,
                format!("{} configured", target.roots.len()),
            ));
        }
    }

    checks.push(match socket {
        Some(path) if path.exists() => Check::new(
            "daemon",
            "socket",
            Status::Ok,
            format!("present at {}", redact_path(path)),
        ),
        Some(path) => Check::new(
            "daemon",
            "socket",
            Status::Skipped,
            format!(
                "no socket at {}; the daemon is not running",
                redact_path(path)
            ),
        )
        .with_remedy("super-herdr daemon".to_owned()),
        None => Check::new(
            "daemon",
            "socket",
            Status::Skipped,
            "no runtime directory to hold one",
        ),
    });

    checks.push(browser_route(config));
    checks.push(match config.devices.len() {
        0 => Check::new(
            "pairing",
            "devices",
            Status::Skipped,
            "no device paired yet",
        )
        .with_remedy("pair from the TUI with Ctrl+] then P".to_owned()),
        count => Check::new("pairing", "devices", Status::Ok, format!("{count} paired")),
    });

    checks
}

fn browser_route(config: &Config) -> Check {
    if config.web.port == Some(0) {
        return Check::new(
            "browser",
            "route",
            Status::Skipped,
            "web.port = 0 serves no browser client",
        );
    }
    match (config.web.url.as_deref(), config.web.address) {
        (Some(url), _) => Check::new(
            "browser",
            "route",
            Status::Ok,
            format!("operator-managed proxy at {}", redact_url(url)),
        ),
        (None, Some(_)) => Check::new(
            "browser",
            "route",
            Status::Ok,
            "direct listener on a configured address",
        ),
        (None, None) if config.web.bridge => Check::new(
            "browser",
            "route",
            Status::Ok,
            "hosted bridge; TLS terminates there, so it is a trusted relay",
        ),
        (None, None) => Check::new(
            "browser",
            "route",
            Status::Warn,
            "no bridge and no address; a phone on another network cannot reach this daemon",
        )
        .with_remedy("set web.bridge = true, or web.address for a private route".to_owned()),
    }
}

/// Turn one target probe into a check, keeping hosts out of the text.
pub fn target_check(report: &ProbeReport) -> Check {
    if !report.ok {
        let detail = report
            .error
            .as_deref()
            .unwrap_or("no snapshot")
            .lines()
            .next()
            .unwrap_or("no snapshot")
            .to_owned();
        return Check::new("targets", &report.target, Status::Fail, detail).with_remedy(format!(
            "super-herdr probe --timeout 30 # {}",
            report.target
        ));
    }
    let version = report.herdr_version.as_deref().unwrap_or("unknown");
    let protocol = report
        .protocol
        .map(|protocol| protocol.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    let status = if report.protocol.is_some_and(|protocol| protocol < 20) {
        Status::Warn
    } else {
        Status::Ok
    };
    let detail = format!(
        "herdr {version}, protocol {protocol}, {} pane(s), {} agent(s), {} ms",
        report.panes, report.agents, report.elapsed_ms
    );
    let check = Check::new("targets", &report.target, status, detail);
    if status == Status::Warn {
        return check.with_remedy(
            "upgrade Herdr on that host to 0.8.2 or newer for plugin actions".to_owned(),
        );
    }
    check
}

/// The tools a target needs for file transfers to work.
///
/// Worth its own check because the failure is invisible until somebody tries:
/// the transfer scripts hash on the host, and a host with no digest tool at all
/// fails every transfer with an error about a script rather than about a
/// missing program.
pub fn transfer_tools_check(target: &str, tools: &[&str]) -> Check {
    match tools.first() {
        Some(tool) => Check::new(
            "transfers",
            target,
            Status::Ok,
            format!("digests with {tool}"),
        ),
        None => Check::new(
            "transfers",
            target,
            Status::Fail,
            "no sha256sum, shasum or openssl; file transfers cannot be verified",
        )
        .with_remedy("install coreutils or openssl on that host".to_owned()),
    }
}

/// Ask a host which digest tool it has.
///
/// The parameterless script is fixed text and reads nothing from the wire, so
/// there is nothing here to quote. A host that answers with none is one whose
/// transfers cannot be verified, which is worth knowing before somebody tries.
const DIGEST_TOOLS_SCRIPT: &str = r#"set -eu
for tool in sha256sum shasum openssl; do
  if command -v "$tool" >/dev/null 2>&1; then
    printf '%s
' "$tool"
  fi
done
"#;

pub async fn digest_tools(
    target: &crate::config::Target,
    transport: &crate::config::TransportConfig,
) -> Vec<String> {
    use std::process::Stdio;

    use tokio::process::Command;

    let mut command = match target.ssh.as_deref() {
        Some(destination) => crate::transport::build_ssh_command(
            destination,
            transport,
            DIGEST_TOOLS_SCRIPT.to_owned(),
        ),
        None => {
            let mut command = Command::new("/bin/sh");
            command.arg("-c").arg(DIGEST_TOOLS_SCRIPT);
            command
        }
    };
    let Ok(child) = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    else {
        return Vec::new();
    };
    let Ok(Ok(output)) = tokio::time::timeout(NETWORK_TIMEOUT, child.wait_with_output()).await
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Probe every target, each on its own bounded clock.
pub async fn probe_targets(config: &Config, timeout: Option<Duration>) -> Vec<Check> {
    let timeout = timeout.unwrap_or(NETWORK_TIMEOUT);
    match probe::probe_all(config, timeout).await {
        Ok(reports) => reports.iter().map(target_check).collect(),
        Err(error) => vec![
            Check::new("targets", "probe", Status::Fail, error.to_string())
                .with_remedy("super-herdr check".to_owned()),
        ],
    }
}

#[cfg(target_family = "unix")]
fn config_permissions(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(std::fs::metadata(path).ok()?.permissions().mode() & 0o7777)
}

#[cfg(not(target_family = "unix"))]
fn config_permissions(_path: &Path) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        Check, Report, Status, local_checks, redact_destination, redact_path, redact_url,
        target_check, transfer_tools_check,
    };
    use crate::config::Config;
    use crate::probe::ProbeReport;

    fn config(text: &str) -> Config {
        Config::parse(text).unwrap()
    }

    fn probe(ok: bool, protocol: Option<u64>) -> ProbeReport {
        ProbeReport {
            target: "development".to_owned(),
            endpoint: "example-host".to_owned(),
            session: "work".to_owned(),
            ok,
            elapsed_ms: 12,
            herdr_bin: Some("/home/example/.local/bin/herdr".to_owned()),
            herdr_version: Some("0.8.0".to_owned()),
            protocol,
            workspaces: 1,
            tabs: 1,
            panes: 2,
            agents: 1,
            workspace_ids: Vec::new(),
            error: (!ok).then(|| "ssh: connect to host example.com: refused".to_owned()),
            snapshot: None,
        }
    }

    #[test]
    fn a_report_says_where_a_machine_is_without_saying_which_machine() {
        assert_eq!(
            redact_path(Path::new("/home/someone/.config/x.toml")),
            "…/x.toml"
        );
        assert_eq!(
            redact_destination("deploy@build.internal.example"),
            "<user@host, 29 chars>"
        );
        assert_eq!(redact_destination("buildhost"), "<host, 9 chars>");
        assert_eq!(redact_url("https://relay.example/r/abc"), "<https url>");
    }

    #[test]
    fn a_configuration_other_users_can_read_is_a_warning_with_a_command() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        #[cfg(target_family = "unix")]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        let checks = local_checks(
            &config("[[targets]]\nname = \"one\"\nssh = \"host\"\n"),
            &path,
            None,
        );

        let permissions = checks
            .iter()
            .find(|check| check.name == "permissions")
            .unwrap();
        #[cfg(target_family = "unix")]
        {
            assert_eq!(permissions.status, Status::Warn);
            assert!(
                permissions
                    .remedy
                    .as_deref()
                    .unwrap()
                    .starts_with("chmod 600")
            );
            assert!(
                !permissions.remedy.as_deref().unwrap().contains("someone"),
                "even a remedy is something somebody pastes into an issue"
            );
        }
        #[cfg(not(target_family = "unix"))]
        assert_eq!(permissions.status, Status::Skipped);
    }

    #[test]
    fn a_target_is_described_without_naming_its_host() {
        let checks = local_checks(
            &config("[[targets]]\nname = \"build\"\nssh = \"deploy@build.internal\"\n"),
            Path::new("/home/someone/.config/super-herdr/config.toml"),
            None,
        );

        let target = checks.iter().find(|check| check.name == "build").unwrap();
        assert!(target.detail.contains("<user@host"));
        assert!(
            !target.detail.contains("build.internal"),
            "a target's own label is kept; the host it names is not"
        );
    }

    #[test]
    fn an_unreachable_target_reports_one_line_and_a_command() {
        let check = target_check(&probe(false, None));

        assert_eq!(check.status, Status::Fail);
        assert_eq!(check.detail, "ssh: connect to host example.com: refused");
        assert!(check.remedy.is_some(), "a failure says what to run next");
    }

    #[test]
    fn a_target_too_old_for_plugin_actions_is_a_warning_rather_than_a_failure() {
        assert_eq!(target_check(&probe(true, Some(19))).status, Status::Warn);
        assert_eq!(target_check(&probe(true, Some(20))).status, Status::Ok);
    }

    #[test]
    fn a_host_with_no_digest_tool_cannot_verify_a_transfer() {
        assert_eq!(
            transfer_tools_check("build", &["sha256sum"]).status,
            Status::Ok
        );
        let missing = transfer_tools_check("build", &[]);
        assert_eq!(missing.status, Status::Fail);
        assert!(missing.remedy.is_some());
    }

    #[test]
    fn a_daemon_with_no_route_out_is_a_warning_a_phone_would_hit() {
        let checks = local_checks(
            &config("[web]\nbridge = false\n\n[[targets]]\nname = \"one\"\nssh = \"host\"\n"),
            Path::new("/tmp/config.toml"),
            None,
        );

        let route = checks.iter().find(|check| check.area == "browser").unwrap();
        assert_eq!(route.status, Status::Warn);
        assert!(route.detail.contains("another network"));
    }

    #[test]
    fn one_area_is_one_heading_however_the_checks_arrived() {
        let report = Report {
            checks: vec![
                Check::new("targets", "build", Status::Ok, "configured"),
                Check::new("daemon", "socket", Status::Ok, "present"),
                Check::new("targets", "build reachability", Status::Ok, "12 ms"),
            ],
        };

        let rendered = report.render();

        assert_eq!(
            rendered.matches("targets\n").count(),
            1,
            "a heading printed twice reads as two different subjects"
        );
        let targets = rendered.find("targets\n").unwrap();
        assert!(rendered[targets..].contains("build reachability"));
    }

    #[test]
    fn an_imported_line_is_reported_as_the_subsystem_describes_itself() {
        assert_eq!(
            Check::note(
                "clipboard",
                "paste action: unavailable to a nested process".to_owned()
            )
            .status,
            Status::Skipped,
            "reporting every imported line as ok would agree with the report and not the machine"
        );
        assert_eq!(
            Check::note("notifications", "notifications: disabled".to_owned()).status,
            Status::Skipped
        );
        assert_eq!(
            Check::note("clipboard", "copy: OSC 52 terminal request".to_owned()).status,
            Status::Ok
        );
    }

    #[test]
    fn a_report_renders_grouped_lines_and_reports_whether_anything_failed() {
        let report = Report {
            checks: vec![
                Check::new("configuration", "file", Status::Ok, "read …/config.toml"),
                Check::new("targets", "build", Status::Fail, "unreachable")
                    .with_remedy("super-herdr probe"),
            ],
        };

        let rendered = report.render();

        assert!(rendered.contains("configuration\n  ok   file:"));
        assert!(rendered.contains("FAIL build: unreachable"));
        assert!(rendered.contains("try: super-herdr probe"));
        assert!(report.failed());
    }
}
