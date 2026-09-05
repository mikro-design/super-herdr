//! Reading the hosts somebody already has in their OpenSSH configuration.
//!
//! Anybody with several machines already wrote them down once. Asking them to
//! write the same names into a second file, correctly, before Super-Herdr works
//! at all is the step where people stop — so this reads what is there and
//! offers it.
//!
//! It offers and never takes. Parsing a configuration is a suggestion; adding a
//! target is a decision, made by naming the aliases wanted. A first run that
//! silently adopted forty hosts, half of them jump boxes and CI runners, would
//! be a federation nobody asked for and a probe against machines nobody meant
//! to touch.
//!
//! What is parsed is deliberately a fraction of the format. `Host` blocks and
//! their `HostName`, `User` and `Port` are enough to show somebody which
//! machine an alias means; `Match` blocks, canonicalisation and the rest change
//! what ssh does without changing which aliases exist, and reimplementing them
//! would mean a second, worse ssh. Nothing here is used to *connect* — that
//! stays ssh's job, given the alias — so a detail this skips cannot change
//! where a connection goes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// How many files an `Include` chain may pull in, and how deep it may go.
/// A configuration that fans out further than this is not one this should be
/// reading at startup.
const MAX_INCLUDE_DEPTH: usize = 3;
const MAX_FILES: usize = 64;
const MAX_HOSTS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHost {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
}

impl SshHost {
    /// One line describing what this alias means, for a preview somebody reads
    /// before choosing.
    pub fn summary(&self) -> String {
        let mut summary = self.alias.clone();
        if let Some(hostname) = &self.hostname {
            let user = self
                .user
                .as_ref()
                .map(|user| format!("{user}@"))
                .unwrap_or_default();
            summary.push_str(&format!("  {user}{hostname}"));
        }
        if let Some(port) = self.port {
            summary.push_str(&format!(":{port}"));
        }
        summary
    }
}

/// Read the aliases from a configuration file and everything it includes.
pub fn load(path: &Path) -> Result<Vec<SshHost>> {
    let mut seen = BTreeSet::new();
    let mut hosts = Vec::new();
    read_into(path, 0, &mut seen, &mut hosts)?;
    hosts.truncate(MAX_HOSTS);
    Ok(hosts)
}

/// Where OpenSSH keeps a user's configuration.
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ssh/config"))
}

fn read_into(
    path: &Path,
    depth: usize,
    seen: &mut BTreeSet<PathBuf>,
    hosts: &mut Vec<SshHost>,
) -> Result<()> {
    if depth > MAX_INCLUDE_DEPTH || seen.len() >= MAX_FILES || !seen.insert(path.to_path_buf()) {
        return Ok(());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    for (host, include) in parse_into(&text) {
        match (host, include) {
            (Some(host), _) => hosts.push(host),
            (None, Some(pattern)) => {
                for included in expand_include(path, &pattern) {
                    // A file that cannot be read is skipped rather than fatal:
                    // an Include naming something absent is ordinary, and
                    // refusing to show any hosts because of it would be worse
                    // than showing the ones that were readable.
                    let _ = read_into(&included, depth + 1, seen, hosts);
                }
            }
            (None, None) => {}
        }
    }
    Ok(())
}

/// Aliases and includes, in the order they appear.
fn parse_into(text: &str) -> Vec<(Option<SshHost>, Option<String>)> {
    let mut found: Vec<(Option<SshHost>, Option<String>)> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (keyword, value) = match line.split_once([' ', '\t', '=']) {
            Some((keyword, value)) => (
                keyword.to_ascii_lowercase(),
                value.trim().trim_start_matches('=').trim(),
            ),
            None => (line.to_ascii_lowercase(), ""),
        };
        match keyword.as_str() {
            "host" => {
                current.clear();
                for alias in value.split_whitespace() {
                    // A pattern is not a host somebody can be offered: `*` and
                    // `web-?` name whatever matches later, and a negation names
                    // what does not. Offering one would produce a target whose
                    // destination ssh cannot resolve.
                    if alias.contains(['*', '?']) || alias.starts_with('!') {
                        continue;
                    }
                    current.push(found.len());
                    found.push((
                        Some(SshHost {
                            alias: alias.to_owned(),
                            hostname: None,
                            user: None,
                            port: None,
                        }),
                        None,
                    ));
                }
            }
            // A Match block changes which options apply, not which aliases
            // exist. Ending the current block is enough: options inside it are
            // not attributed to the last Host, which would misdescribe it.
            "match" => current.clear(),
            "include" => found.push((None, Some(value.to_owned()))),
            "hostname" | "user" | "port" => {
                for index in &current {
                    let Some((Some(host), _)) = found.get_mut(*index) else {
                        continue;
                    };
                    match keyword.as_str() {
                        "hostname" => host.hostname = Some(value.to_owned()),
                        "user" => host.user = Some(value.to_owned()),
                        _ => host.port = value.parse().ok(),
                    }
                }
            }
            _ => {}
        }
    }
    found
}

/// Turn one `Include` value into the files it names.
///
/// Relative paths are relative to the including file's directory, which is what
/// ssh does. Globs are expanded only one level and only by literal directory
/// reading, because the alternative is a glob crate for a feature that lists
/// somebody's own files.
fn expand_include(from: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut expanded = Vec::new();
    for entry in pattern.split_whitespace() {
        let entry = entry.trim_matches('"');
        let path = if let Some(rest) = entry.strip_prefix("~/") {
            let Some(home) = std::env::var_os("HOME") else {
                continue;
            };
            PathBuf::from(home).join(rest)
        } else if entry.starts_with('/') {
            PathBuf::from(entry)
        } else {
            from.parent().unwrap_or(Path::new(".")).join(entry)
        };
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        if !name.contains(['*', '?']) {
            expanded.push(path);
            continue;
        }
        let Some(directory) = path.parent() else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        let prefix = name.split(['*', '?']).next().unwrap_or_default().to_owned();
        for entry in entries.flatten() {
            let candidate = entry.file_name().to_string_lossy().into_owned();
            if candidate.starts_with(&prefix) && entry.path().is_file() {
                expanded.push(entry.path());
            }
        }
    }
    expanded.sort();
    expanded
}

#[cfg(test)]
mod tests {
    use super::{SshHost, load, parse_into};

    fn hosts(text: &str) -> Vec<SshHost> {
        parse_into(text)
            .into_iter()
            .filter_map(|(host, _)| host)
            .collect()
    }

    #[test]
    fn aliases_carry_enough_to_tell_which_machine_they_mean() {
        let parsed =
            hosts("Host build\n  HostName build.internal.example\n  User deploy\n  Port 2222\n");

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].alias, "build");
        assert_eq!(
            parsed[0].summary(),
            "build  deploy@build.internal.example:2222"
        );
    }

    #[test]
    fn a_pattern_is_not_a_host_anybody_can_be_offered() {
        let parsed = hosts("Host *\n  User default\n\nHost web-?\n\nHost !secret prod\n");

        assert_eq!(
            parsed
                .iter()
                .map(|host| host.alias.as_str())
                .collect::<Vec<_>>(),
            ["prod"],
            "a wildcard names whatever matches later, which is not a machine to add"
        );
    }

    #[test]
    fn one_block_naming_several_aliases_produces_several_hosts() {
        let parsed = hosts("Host build ci\n  HostName shared.example\n");

        assert_eq!(parsed.len(), 2);
        assert!(
            parsed
                .iter()
                .all(|host| host.hostname.as_deref() == Some("shared.example")),
            "options in a block belong to every alias it named"
        );
    }

    #[test]
    fn options_after_a_match_block_are_not_attributed_to_the_last_host() {
        let parsed =
            hosts("Host build\n  HostName build.example\n\nMatch host anything\n  User root\n");

        assert_eq!(parsed[0].hostname.as_deref(), Some("build.example"));
        assert_eq!(
            parsed[0].user, None,
            "a Match block changes which options apply, not which host they describe"
        );
    }

    #[test]
    fn keywords_are_case_insensitive_and_may_use_equals() {
        let parsed = hosts("HOST=build\n  hostname=build.example\n  PORT = 2200\n");

        assert_eq!(parsed[0].alias, "build");
        assert_eq!(parsed[0].hostname.as_deref(), Some("build.example"));
        assert_eq!(parsed[0].port, Some(2200));
    }

    #[test]
    fn comments_and_blank_lines_are_not_hosts() {
        assert!(hosts("# Host commented\n\n   \n").is_empty());
    }

    #[test]
    fn an_include_pulls_in_the_hosts_it_names() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("extra"),
            "Host lab\n  HostName lab.example\n",
        )
        .unwrap();
        let main = directory.path().join("config");
        std::fs::write(&main, "Include extra\n\nHost build\n").unwrap();

        let parsed = load(&main).unwrap();

        assert_eq!(
            parsed
                .iter()
                .map(|host| host.alias.as_str())
                .collect::<Vec<_>>(),
            ["lab", "build"]
        );
    }

    #[test]
    fn an_include_that_names_nothing_readable_does_not_hide_the_rest() {
        let directory = tempfile::tempdir().unwrap();
        let main = directory.path().join("config");
        std::fs::write(&main, "Include absent\n\nHost build\n").unwrap();

        let parsed = load(&main).unwrap();

        assert_eq!(
            parsed
                .iter()
                .map(|host| host.alias.as_str())
                .collect::<Vec<_>>(),
            ["build"],
            "an Include naming something absent is ordinary"
        );
    }

    #[test]
    fn a_configuration_that_includes_itself_terminates() {
        let directory = tempfile::tempdir().unwrap();
        let main = directory.path().join("config");
        std::fs::write(&main, "Include config\n\nHost build\n").unwrap();

        let parsed = load(&main).unwrap();

        assert_eq!(parsed.len(), 1);
    }
}
