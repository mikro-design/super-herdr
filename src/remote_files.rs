//! Browsing and searching a target's files, inside bounds somebody configured.
//!
//! This exists so a person on a phone can find a file without typing its whole
//! path from memory. It is deliberately not a file manager: it lists one
//! directory, searches for file names under one root, and does nothing else.
//! Reading a file is the transfer protocol's job and stays there.
//!
//! Three properties hold, and each is enforced where it cannot be argued with:
//!
//! * **Nothing is interpolated into a shell.** Every parameter travels on the
//!   remote script's stdin and is read into a variable, the same way the file
//!   read already works. A path with a quote, a semicolon or a newline in it is
//!   a path, not a command.
//! * **Nothing leaves the root.** The script changes into the directory and
//!   compares `pwd -P` — the *physical* path, with symlinks resolved — against
//!   the root's own physical path. A symlink pointing out of the tree resolves
//!   to somewhere that fails that comparison, so it is refused rather than
//!   followed. Search does not follow symlinks at all.
//! * **Everything is bounded.** How many roots a target may declare, how deep a
//!   search goes, how many entries come back, how long a query may be, and how
//!   many bytes the remote may produce. A directory with a million files
//!   returns a page and says it was truncated.
//!
//! Entries carry a name and a kind, and no size. Size is what the transfer
//! offer already reports, and it reports it at the moment it matters — before
//! any byte moves. Collecting it here would mean a `wc -c` per entry, which on
//! some hosts reads every file in the directory to answer a question the next
//! step answers for free.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::config::{Target, TransportConfig};
use crate::transport::build_ssh_command;

/// How many entries one listing may carry.
const MAX_ENTRIES: usize = 500;
/// How deep a search descends below its root.
const MAX_SEARCH_DEPTH: u32 = 6;
/// How many bytes the remote side may produce for one answer.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_QUERY_CHARACTERS: usize = 128;
const MAX_RELATIVE_CHARACTERS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteEntryKind {
    Directory,
    File,
    /// A socket, device, or anything else that is neither. Listed so a person
    /// can see it is there, and never offered as something to fetch.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteEntry {
    pub kind: RemoteEntryKind,
    /// A single name when listing a directory, or a path relative to the root
    /// when searching. Never absolute: what a client is shown stays inside the
    /// root it asked about.
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteListing {
    pub root: String,
    /// Where inside the root this describes. Empty is the root itself.
    pub path: String,
    pub entries: Vec<RemoteEntry>,
    /// Whether there was more than this answer could carry. Said plainly,
    /// because a list silently missing the file somebody is looking for is
    /// worse than one that admits it stopped.
    pub truncated: bool,
}

/// Read one directory inside a root.
const LIST_SCRIPT: &str = r#"set -eu
IFS= read -r root
IFS= read -r relative
cd "$root" 2>/dev/null || exit 3
root_real=$(pwd -P)
if [ -n "$relative" ]; then
  cd "./$relative" 2>/dev/null || exit 4
fi
here=$(pwd -P)
case "$here" in
  "$root_real") ;;
  "$root_real"/*) ;;
  *) exit 5 ;;
esac
count=0
for entry in * .[!.]* ..?*; do
  if [ ! -e "$entry" ] && [ ! -L "$entry" ]; then
    continue
  fi
  if [ -d "$entry" ]; then
    kind=d
  elif [ -f "$entry" ]; then
    kind=f
  else
    kind=o
  fi
  printf '%s\0%s\0' "$kind" "$entry"
  count=$((count + 1))
  if [ "$count" -ge 500 ]; then
    exit 0
  fi
done
"#;

/// Find file names under a root, without following symlinks out of it.
const SEARCH_SCRIPT: &str = r#"set -eu
IFS= read -r root
IFS= read -r pattern
IFS= read -r depth
cd "$root" 2>/dev/null || exit 3
find . -maxdepth "$depth" -name "$pattern" -type f -print0 2>/dev/null
"#;

/// List one directory, given as a path relative to one of the target's roots.
pub async fn list(
    target: &Target,
    transport: &TransportConfig,
    root: &str,
    relative: &str,
) -> Result<RemoteListing> {
    let root = permitted_root(target, root)?;
    let relative = checked_relative(relative)?;
    let output = run(
        target,
        transport,
        LIST_SCRIPT,
        &[&root, &relative],
        "list a directory",
    )
    .await?;
    let (entries, truncated) = parse_entries(&output);
    Ok(RemoteListing {
        root,
        path: relative,
        entries,
        truncated,
    })
}

/// Find files under a root by name.
///
/// `glob` is what makes the query a pattern. Without it the query matches as
/// literal text anywhere in a name, and the characters a glob would treat
/// specially are escaped before they reach the host — so somebody searching
/// for `[` finds a file called `[` rather than an error or a wildcard.
pub async fn search(
    target: &Target,
    transport: &TransportConfig,
    root: &str,
    query: &str,
    glob: bool,
) -> Result<RemoteListing> {
    let root = permitted_root(target, root)?;
    ensure!(
        !query.is_empty()
            && query.chars().count() <= MAX_QUERY_CHARACTERS
            && !query.chars().any(char::is_control),
        "a search must be 1 to {MAX_QUERY_CHARACTERS} characters of ordinary text"
    );
    let pattern = if glob {
        query.to_owned()
    } else {
        format!("*{}*", escape_glob(query))
    };
    let output = run(
        target,
        transport,
        SEARCH_SCRIPT,
        &[&root, &pattern, &MAX_SEARCH_DEPTH.to_string()],
        "search for files",
    )
    .await?;
    let (mut entries, truncated) = parse_paths(&output);
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(RemoteListing {
        root,
        path: String::new(),
        entries,
        truncated,
    })
}

/// The roots a target declares, as a client should see them.
pub fn roots(target: &Target) -> Vec<String> {
    target.roots.clone()
}

/// Check a root against what the target declares.
///
/// A client names a root rather than describing one: the answer has to be one
/// of the paths whoever owns the configuration wrote down, compared exactly.
/// Accepting a prefix or a parent would let a client widen its own bounds.
fn permitted_root(target: &Target, root: &str) -> Result<String> {
    target
        .roots
        .iter()
        .find(|declared| declared.as_str() == root)
        .cloned()
        .with_context(|| "that path is not one of this target's configured roots".to_owned())
}

/// A relative path this is willing to descend into.
///
/// Refusing `..` here is belt to the script's braces: the script compares
/// physical paths and would catch an escape anyway, but a request that is
/// obviously trying to leave should not reach a host at all.
fn checked_relative(relative: &str) -> Result<String> {
    let relative = relative.trim_start_matches('/');
    ensure!(
        relative.chars().count() <= MAX_RELATIVE_CHARACTERS
            && !relative.chars().any(char::is_control),
        "that path is too long or contains control characters"
    );
    ensure!(
        !relative.split('/').any(|part| part == ".."),
        "a path inside a root may not climb out of it"
    );
    Ok(relative.to_owned())
}

/// Make a query match as text rather than as a pattern.
fn escape_glob(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len());
    for character in query.chars() {
        if matches!(character, '*' | '?' | '[' | ']' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn parse_entries(output: &[u8]) -> (Vec<RemoteEntry>, bool) {
    let mut fields = split_nul(output);
    let mut entries = Vec::new();
    while let (Some(kind), Some(name)) = (fields.next(), fields.next()) {
        let Ok(name) = String::from_utf8(name.to_vec()) else {
            continue;
        };
        entries.push(RemoteEntry {
            kind: match kind {
                b"d" => RemoteEntryKind::Directory,
                b"f" => RemoteEntryKind::File,
                _ => RemoteEntryKind::Other,
            },
            name,
        });
    }
    let truncated = entries.len() >= MAX_ENTRIES;
    entries.truncate(MAX_ENTRIES);
    (entries, truncated)
}

fn parse_paths(output: &[u8]) -> (Vec<RemoteEntry>, bool) {
    let mut entries = Vec::new();
    for path in split_nul(output) {
        let Ok(path) = String::from_utf8(path.to_vec()) else {
            continue;
        };
        let name = path.strip_prefix("./").unwrap_or(&path).to_owned();
        if name.is_empty() {
            continue;
        }
        entries.push(RemoteEntry {
            kind: RemoteEntryKind::File,
            name,
        });
    }
    let truncated = entries.len() >= MAX_ENTRIES;
    entries.truncate(MAX_ENTRIES);
    (entries, truncated)
}

/// Split NUL-terminated output, discarding anything after the last terminator.
///
/// The remote side is capped by bytes, so its last record may have been cut in
/// half. A half a file name is not a file name, and dropping it is why the
/// framing is NUL rather than newlines in the first place.
fn split_nul(output: &[u8]) -> impl Iterator<Item = &[u8]> {
    let complete = output
        .iter()
        .rposition(|byte| *byte == 0)
        .map_or(0, |index| index + 1);
    output[..complete]
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
}

async fn run(
    target: &Target,
    transport: &TransportConfig,
    script: &str,
    arguments: &[&str],
    what: &str,
) -> Result<Vec<u8>> {
    let mut command = match target.ssh.as_deref() {
        Some(destination) => build_ssh_command(destination, transport, script.to_owned()),
        None => {
            let mut command = Command::new("/bin/sh");
            command.arg("-c").arg(script);
            command
        }
    };
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to start the command to {what}"))?;
    let mut input = child
        .stdin
        .take()
        .context("the remote command did not expose stdin")?;
    // One line per parameter, in the order the script reads them. Every one of
    // them is data: the script reads into a variable and never evaluates.
    let mut written = String::new();
    for argument in arguments {
        written.push_str(argument);
        written.push('\n');
    }
    let _ = input.write_all(written.as_bytes()).await;
    let _ = input.shutdown().await;
    drop(input);

    let output = timeout(
        Duration::from_secs(transport.command_timeout_seconds),
        child.wait_with_output(),
    )
    .await
    .with_context(|| format!("timed out trying to {what}"))?
    .with_context(|| format!("failed to {what}"))?;
    if !output.status.success() {
        bail!("could not {what} on that target");
    }
    let mut bytes = output.stdout;
    bytes.truncate(MAX_OUTPUT_BYTES);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_ENTRIES, RemoteEntryKind, checked_relative, escape_glob, parse_entries, parse_paths,
        permitted_root,
    };
    use crate::config::Target;

    fn target(roots: &[&str]) -> Target {
        Target {
            name: "host-a".to_owned(),
            ssh: Some("host".to_owned()),
            discover_sessions: false,
            session: None,
            socket: None,
            herdr_bins: Vec::new(),
            roots: roots.iter().map(|root| (*root).to_owned()).collect(),
        }
    }

    #[test]
    fn a_root_must_be_one_the_target_declared() {
        let declared = target(&["/srv/build", "/home/example/work"]);
        let none = target(&[]);

        assert_eq!(
            permitted_root(&declared, "/srv/build").unwrap(),
            "/srv/build"
        );
        assert!(
            permitted_root(&declared, "/srv").is_err(),
            "a parent of a root is not a root"
        );
        assert!(
            permitted_root(&declared, "/srv/build/inner").is_err(),
            "a client naming a deeper path must still name the root it came from"
        );
        assert!(permitted_root(&declared, "/etc").is_err());
        assert!(
            permitted_root(&none, "/srv").is_err(),
            "a target that declared no roots offers none"
        );
    }

    #[test]
    fn a_path_inside_a_root_may_not_climb_out_of_it() {
        assert_eq!(checked_relative("sub/dir").unwrap(), "sub/dir");
        assert_eq!(
            checked_relative("/sub/dir").unwrap(),
            "sub/dir",
            "a leading separator is a client's habit, not a request for the filesystem root"
        );
        assert!(checked_relative("../etc").is_err());
        assert!(checked_relative("sub/../../etc").is_err());
        assert!(
            checked_relative("sub/..name").is_ok(),
            "a name that merely starts with dots is not a climb"
        );
    }

    #[test]
    fn a_plain_search_matches_text_and_not_a_pattern() {
        assert_eq!(escape_glob("report[1].txt"), r"report\[1\].txt");
        assert_eq!(escape_glob("*.rs"), r"\*.rs");
        assert_eq!(escape_glob("plain"), "plain");
    }

    #[test]
    fn entries_carry_their_kind_and_survive_awkward_names() {
        let output = b"d\0sub dir\0f\0report.txt\0f\0line\nbreak.txt\0o\0socket\0";

        let (entries, truncated) = parse_entries(output);

        assert!(!truncated);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].kind, RemoteEntryKind::Directory);
        assert_eq!(entries[0].name, "sub dir");
        assert_eq!(
            entries[2].name, "line\nbreak.txt",
            "NUL framing is what lets a name contain a newline"
        );
        assert_eq!(entries[3].kind, RemoteEntryKind::Other);
    }

    #[test]
    fn a_record_cut_in_half_by_the_byte_cap_is_dropped() {
        let output = b"f\0whole.txt\0f\0half-a-na";

        let (entries, _) = parse_entries(output);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "whole.txt");
    }

    #[test]
    fn a_listing_says_when_it_stopped_short() {
        let mut output = Vec::new();
        for index in 0..MAX_ENTRIES + 1 {
            output.extend_from_slice(format!("f\0file{index}\0").as_bytes());
        }

        let (entries, truncated) = parse_entries(&output);

        assert_eq!(entries.len(), MAX_ENTRIES);
        assert!(
            truncated,
            "a list silently missing the file somebody wants is worse than one that admits it"
        );
    }

    #[test]
    fn search_results_are_relative_to_the_root_they_came_from() {
        let output = b"./sub/report.txt\0./top.txt\0";

        let (entries, _) = parse_paths(output);

        assert_eq!(entries[0].name, "sub/report.txt");
        assert_eq!(entries[1].name, "top.txt");
        assert!(entries.iter().all(|entry| !entry.name.starts_with('/')));
    }
}
