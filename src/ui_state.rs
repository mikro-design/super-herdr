use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::PaneId;

const UI_STATE_VERSION: u32 = 1;
const MAX_UI_STATE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiState {
    version: u32,
    pub selected_pane: Option<PaneId>,
}

impl UiState {
    pub fn selected_pane(selected_pane: PaneId) -> Self {
        Self {
            version: UI_STATE_VERSION,
            selected_pane: Some(selected_pane),
        }
    }
}

#[derive(Debug, Clone)]
pub struct UiStateStore {
    path: PathBuf,
}

impl UiStateStore {
    pub fn discover() -> Result<Self> {
        let root = if let Some(root) = env::var_os("XDG_STATE_HOME") {
            PathBuf::from(root)
        } else {
            let home: OsString = env::var_os("HOME")
                .context("XDG_STATE_HOME or HOME is required to persist Super-Herdr UI state")?;
            PathBuf::from(home).join(".local/state")
        };
        Ok(Self {
            path: root.join("super-herdr/ui-state.json"),
        })
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<UiState> {
        load_from(&self.path)
    }

    pub fn save(&self, state: &UiState) -> Result<()> {
        save_to(&self.path, state)
    }
}

fn load_from(path: &Path) -> Result<UiState> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(UiState::default());
        }
        Err(error) => return Err(error).context("failed to inspect UI state"),
    };
    if metadata.len() > MAX_UI_STATE_BYTES {
        bail!("persisted UI state exceeds the size limit");
    }
    let bytes = fs::read(path).context("failed to read persisted UI state")?;
    let state: UiState = serde_json::from_slice(&bytes).context("persisted UI state is invalid")?;
    if state.version != UI_STATE_VERSION {
        bail!("persisted UI state has an unsupported version");
    }
    Ok(state)
}

fn save_to(path: &Path, state: &UiState) -> Result<()> {
    let directory = path
        .parent()
        .context("persisted UI state path has no parent directory")?;
    fs::create_dir_all(directory).context("failed to create the UI state directory")?;
    set_directory_permissions(directory)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".ui-state-")
        .tempfile_in(directory)
        .context("failed to create a temporary UI state file")?;
    set_file_permissions(temporary.path())?;
    serde_json::to_writer(&mut temporary, state).context("failed to encode UI state")?;
    temporary
        .write_all(b"\n")
        .context("failed to finish UI state")?;
    temporary
        .as_file()
        .sync_all()
        .context("failed to synchronize UI state")?;
    temporary
        .persist(path)
        .context("failed to atomically replace UI state")?;
    Ok(())
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .context("failed to secure the UI state directory")
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .context("failed to secure the UI state file")
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{UiState, UiStateStore};
    use crate::model::PaneId;

    #[test]
    fn atomically_round_trips_a_qualified_pane() {
        let directory = tempfile::tempdir().unwrap();
        let store = UiStateStore::at(directory.path().join("ui-state.json"));
        let state = UiState::selected_pane(PaneId::new("host-a", "work", "w6:p1"));

        store.save(&state).unwrap();

        assert_eq!(store.load().unwrap(), state);
    }

    #[test]
    fn missing_state_is_empty_and_corrupt_state_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ui-state.json");
        let store = UiStateStore::at(path.clone());
        assert_eq!(store.load().unwrap(), UiState::default());

        fs::write(path, b"not json").unwrap();
        assert!(store.load().is_err());
    }
}
