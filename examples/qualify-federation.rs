//! Prove a mixed-version fleet, and that each version's events actually arrive.
//!
//! Unit tests drive a fake transport, and a fake cannot disagree with the real
//! Herdr about what a subscription means. Herdr 0.9 changed exactly that — a
//! subscription now starts at the live edge instead of replaying retained
//! history — so the only way to know this client keeps up is to watch a real
//! server of each version being changed underneath it.
//!
//! Usage: cargo run --example qualify-federation -- [config-path]
//!
//! Every configured target is reported with the Herdr version and protocol it
//! answered on, and whether it reached event-driven updates or fell back to
//! polling. Then each target with a live pane is changed through Herdr's own
//! CLI and the wait until this client sees it is measured. A change seen inside
//! the refresh interval is a change an event delivered; one that takes the full
//! interval is polling wearing an event's clothes, which is precisely the
//! failure the subscription ordering exists to prevent.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use super_herdr::config::Config;
use super_herdr::state::{FederationStore, SupervisorOptions, TargetUpdateMode};
use super_herdr::transport::CliSnapshotTransport;

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args().nth(1);
    let (config, _) = Config::load(path.as_ref().map(std::path::Path::new))
        .context("a configuration with at least one target")?;
    let targets = config.targets.clone();
    anyhow::ensure!(!targets.is_empty(), "no targets are configured");

    let refresh = Duration::from_secs(5);
    let store = FederationStore::start(
        config,
        std::sync::Arc::new(CliSnapshotTransport),
        SupervisorOptions {
            command_timeout: Duration::from_secs(20),
            refresh_interval: refresh,
            initial_backoff: Duration::from_millis(250),
            maximum_backoff: Duration::from_secs(4),
        },
    );
    let mut state = store.subscribe();

    // Long enough for every target to answer once and settle on a mode.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let mut versions: BTreeMap<String, (String, u64, TargetUpdateMode, usize)> = BTreeMap::new();
    {
        let held = state.borrow_and_update();
        for (key, runtime) in &held.targets {
            let snapshot = runtime.snapshot.as_ref();
            versions.insert(
                key.to_string(),
                (
                    snapshot
                        .and_then(|snapshot| snapshot.server_version.clone())
                        .unwrap_or_else(|| "unknown".to_owned()),
                    snapshot.and_then(|snapshot| snapshot.protocol).unwrap_or(0),
                    runtime.update_mode,
                    snapshot.map(|snapshot| snapshot.panes.len()).unwrap_or(0),
                ),
            );
        }
    }

    println!("== the fleet as this client sees it ==");
    for (key, (version, protocol, mode, panes)) in &versions {
        let plugins = if *protocol >= 20 { "with" } else { "without" };
        println!(
            "  {key}: herdr {version}, protocol {protocol}, {mode:?} updates, {panes} pane(s), {plugins} plugin actions"
        );
    }
    let distinct = versions
        .values()
        .map(|(version, ..)| version.clone())
        .collect::<std::collections::BTreeSet<_>>();
    println!(
        "  -> {} distinct Herdr version(s) in one federation: {}",
        distinct.len(),
        distinct.into_iter().collect::<Vec<_>>().join(", ")
    );

    println!("== does a change arrive as an event, per version ==");
    for (key, (version, _, mode, _)) in &versions {
        if *mode != TargetUpdateMode::Events {
            println!("  {key}: on {mode:?}, so nothing to measure — event stream unavailable");
            continue;
        }
        let revision_before = state.borrow_and_update().revision;
        // Changed through Herdr's own CLI rather than through this client, so
        // what is measured is a change this client did not make and has no
        // reason to expect.
        let session = key.split('/').nth(1).unwrap_or_default().to_owned();
        let target = targets
            .iter()
            .find(|target| key.starts_with(&target.name))
            .context("a configured target for a reported one")?;
        let herdr = target
            .herdr_bins
            .first()
            .cloned()
            .unwrap_or_else(|| "herdr".to_owned());
        let label = format!("qualify-{}", std::process::id());
        let created = tokio::process::Command::new(&herdr)
            .args([
                "--session",
                &session,
                "workspace",
                "create",
                "--label",
                &label,
            ])
            .kill_on_drop(true)
            .output()
            .await;
        let created = match created {
            Ok(output) if output.status.success() => output.stdout,
            Ok(output) => {
                println!(
                    "  {key}: could not create a workspace to observe ({}), skipping",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                continue;
            }
            Err(error) => {
                println!("  {key}: could not run {herdr} ({error}), skipping");
                continue;
            }
        };
        let started = Instant::now();
        let saw = tokio::time::timeout(refresh.mul_f32(0.8), async {
            loop {
                if state.changed().await.is_err() {
                    return false;
                }
                if state.borrow_and_update().revision > revision_before {
                    return true;
                }
            }
        })
        .await;
        match saw {
            Ok(true) => println!(
                "  {key}: herdr {version} — change seen in {:?}, inside the {refresh:?} refresh interval, so an event carried it",
                started.elapsed()
            ),
            _ => println!(
                "  {key}: herdr {version} — no change within {:?}; an event did not arrive and polling is what would have corrected it",
                refresh.mul_f32(0.8)
            ),
        }

        // Closed again whatever the measurement said. A qualification run that
        // leaves workspaces behind in somebody's live session is one nobody
        // will run twice, and the session this observes is a real one.
        let workspace = serde_json::from_slice::<serde_json::Value>(&created)
            .ok()
            .and_then(|created| {
                created
                    .pointer("/result/workspace/workspace_id")
                    .and_then(|id| id.as_str())
                    .map(str::to_owned)
            });
        match workspace {
            Some(workspace) => {
                let closed = tokio::process::Command::new(&herdr)
                    .args(["--session", &session, "workspace", "close", &workspace])
                    .kill_on_drop(true)
                    .output()
                    .await;
                let gone = closed
                    .map(|closed| closed.status.success())
                    .unwrap_or(false);
                println!(
                    "    cleanup: workspace {workspace} {}",
                    if gone {
                        "closed"
                    } else {
                        "NOT closed — close it by hand"
                    }
                );
            }
            None => println!("    cleanup: could not read the new workspace id; check by hand"),
        }
    }

    store.shutdown().await;
    Ok(())
}
