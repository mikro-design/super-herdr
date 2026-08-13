use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use tokio::task::JoinSet;

use crate::config::{Config, Target, TransportConfig};
use crate::model::WorkspaceId;
use crate::transport::{CliSnapshotTransport, SnapshotTransport, TransportSnapshot};

#[derive(Debug, Serialize)]
pub struct FederationReport {
    pub config: String,
    pub targets: Vec<ProbeReport>,
}

#[derive(Debug, Serialize)]
pub struct ProbeReport {
    pub target: String,
    pub endpoint: String,
    pub session: String,
    pub ok: bool,
    pub elapsed_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub herdr_bin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub herdr_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u64>,
    pub workspaces: usize,
    pub tabs: usize,
    pub panes: usize,
    pub agents: usize,
    pub workspace_ids: Vec<WorkspaceId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Value>,
}

impl ProbeReport {
    pub fn discard_snapshot(&mut self) {
        self.snapshot = None;
    }
}

pub async fn probe_all(config: &Config, command_timeout: Duration) -> Result<Vec<ProbeReport>> {
    probe_all_with_transport(config, command_timeout, Arc::new(CliSnapshotTransport)).await
}

pub async fn probe_all_with_transport<T>(
    config: &Config,
    command_timeout: Duration,
    snapshot_transport: Arc<T>,
) -> Result<Vec<ProbeReport>>
where
    T: SnapshotTransport,
{
    let mut tasks = JoinSet::new();
    for target in config.targets.iter().cloned() {
        let transport_config = config.transport.clone();
        let snapshot_transport = Arc::clone(&snapshot_transport);
        tasks.spawn(async move {
            probe_target(
                target,
                transport_config,
                command_timeout,
                snapshot_transport,
            )
            .await
        });
    }

    let mut reports = Vec::with_capacity(config.targets.len());
    while let Some(result) = tasks.join_next().await {
        reports.push(result.context("a target probe task failed")?);
    }
    reports.sort_unstable_by(|left, right| {
        left.target
            .cmp(&right.target)
            .then_with(|| left.session.cmp(&right.session))
    });
    Ok(reports)
}

async fn probe_target<T>(
    target: Target,
    transport_config: TransportConfig,
    command_timeout: Duration,
    snapshot_transport: Arc<T>,
) -> ProbeReport
where
    T: SnapshotTransport,
{
    let started = Instant::now();
    let result = snapshot_transport
        .snapshot(&target, &transport_config, command_timeout)
        .await;
    let elapsed_ms = started.elapsed().as_millis();

    match result {
        Ok(selection) => successful_report(&target, elapsed_ms, selection),
        Err(error) => failed_report(&target, elapsed_ms, error.to_string()),
    }
}

fn successful_report(
    target: &Target,
    elapsed_ms: u128,
    selection: TransportSnapshot,
) -> ProbeReport {
    let snapshot = selection.snapshot;
    ProbeReport {
        target: target.name.clone(),
        endpoint: target.endpoint().to_owned(),
        session: target.session_name().to_owned(),
        ok: true,
        elapsed_ms,
        herdr_bin: Some(selection.herdr_bin),
        herdr_version: snapshot
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_owned),
        protocol: snapshot.get("protocol").and_then(Value::as_u64),
        workspaces: array_len(&snapshot, "workspaces"),
        tabs: array_len(&snapshot, "tabs"),
        panes: array_len(&snapshot, "panes"),
        agents: array_len(&snapshot, "agents"),
        workspace_ids: qualified_ids(&snapshot, "workspaces", "workspace_id", target),
        error: None,
        snapshot: Some(snapshot),
    }
}

fn failed_report(target: &Target, elapsed_ms: u128, error: String) -> ProbeReport {
    ProbeReport {
        target: target.name.clone(),
        endpoint: target.endpoint().to_owned(),
        session: target.session_name().to_owned(),
        ok: false,
        elapsed_ms,
        herdr_bin: None,
        herdr_version: None,
        protocol: None,
        workspaces: 0,
        tabs: 0,
        panes: 0,
        agents: 0,
        workspace_ids: Vec::new(),
        error: Some(compact_text(&error)),
        snapshot: None,
    }
}

fn array_len(value: &Value, key: &str) -> usize {
    value.get(key).and_then(Value::as_array).map_or(0, Vec::len)
}

fn qualified_ids(
    value: &Value,
    collection: &str,
    id_key: &str,
    target: &Target,
) -> Vec<WorkspaceId> {
    value
        .get(collection)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get(id_key).and_then(Value::as_str))
        .map(|resource| WorkspaceId::new(&target.name, target.session_name(), resource))
        .collect()
}

fn compact_text(value: &str) -> String {
    const LIMIT: usize = 500;
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= LIMIT {
        return normalized;
    }
    let mut shortened = normalized.chars().take(LIMIT).collect::<String>();
    shortened.push('…');
    shortened
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::successful_report;
    use crate::config::Config;
    use crate::transport::TransportSnapshot;

    #[test]
    fn summarizes_snapshot_without_assuming_optional_fields() {
        let config = Config::parse("[[targets]]\nname = 'host-a'").unwrap();
        let report = successful_report(
            &config.targets[0],
            1,
            TransportSnapshot {
                herdr_bin: "herdr-0.7".to_owned(),
                snapshot: json!({
                    "protocol": 17,
                    "workspaces": [{"workspace_id": "w1"}, {"workspace_id": "w2"}],
                    "panes": [{}]
                }),
            },
        );

        assert_eq!(report.protocol, Some(17));
        assert_eq!(report.workspaces, 2);
        assert_eq!(report.panes, 1);
        assert_eq!(report.agents, 0);
        assert_eq!(report.herdr_bin.as_deref(), Some("herdr-0.7"));
        assert_eq!(report.workspace_ids[0].to_string(), "host-a/default/w1");
    }
}
