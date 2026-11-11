//! Small persisted UI state, stored as JSON in
//! `~/Library/Application Support/cirrus/state.json`.
//!
//! Every field has a default, so new fields can be added without breaking old files.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppState {
    /// Keys of navigator nodes that were expanded (see the app's `NodeKind::key`).
    pub expanded_nodes: Vec<String>,
}

/// Serialises read-modify-write cycles within the process.
static LOCK: Mutex<()> = Mutex::new(());

fn path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/cirrus/state.json"))
}

/// Loads the saved state; a missing or unreadable file yields the defaults.
pub fn load() -> AppState {
    let _guard = LOCK.lock().unwrap();
    read().unwrap_or_default()
}

/// Applies `f` to the saved state and writes it back.
pub fn update(f: impl FnOnce(&mut AppState)) {
    let _guard = LOCK.lock().unwrap();
    let mut state = read().unwrap_or_default();
    f(&mut state);
    if let Err(e) = write(&state) {
        log::warn!("failed to save app state: {e:#}");
    }
}

fn read() -> Option<AppState> {
    let text = std::fs::read_to_string(path()?).ok()?;
    serde_json::from_str(&text).ok()
}

fn write(state: &AppState) -> Result<()> {
    let path = path().context("HOME is not set")?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    // Write then rename so a crash never leaves a half-written file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
