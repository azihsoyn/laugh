//! Settings that last from one run of laugh to the next: whether viewed
//! files are hidden, and where each screen's divider was dragged to. Kept in a small JSON file:
//! `~/Library/Application Support/laugh/` on macOS, `$XDG_STATE_HOME/laugh/`
//! (`~/.local/state/laugh/`) elsewhere, `%LOCALAPPDATA%\laugh\` on Windows;
//! `LAUGH_STATE_FILE` puts it elsewhere.
//!
//! A missing or unreadable file just means the defaults, and a failed save
//! is ignored: none of this is worth stopping a review for.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// `H` on the Files screen.
    pub hide_viewed: bool,
    /// Each screen's left pane share, in thousandths, once dragged.
    pub files_split: Option<u16>,
    pub threads_split: Option<u16>,
    pub checks_split: Option<u16>,
}

pub fn path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("LAUGH_STATE_FILE").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let home = || {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
    };
    let dir = if cfg!(target_os = "macos") {
        home()?.join("Library/Application Support/laugh")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("laugh")
    } else if let Some(state) = std::env::var_os("XDG_STATE_HOME").filter(|p| !p.is_empty()) {
        PathBuf::from(state).join("laugh")
    } else {
        home()?.join(".local/state/laugh")
    };
    Some(dir.join("state.json"))
}

pub fn load() -> Prefs {
    path()
        .and_then(|p| fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(prefs: &Prefs) {
    let Some(path) = path() else { return };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec_pretty(prefs) {
        // Write beside it and rename, so a crash mid-write can't leave
        // half a file.
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_or_missing_fields_fall_back_to_defaults() {
        let p: Prefs = serde_json::from_str(r#"{"hide_viewed": true, "later": 1}"#).unwrap();
        assert!(p.hide_viewed);
        let p: Prefs = serde_json::from_str("{}").unwrap();
        assert_eq!(p, Prefs::default());
    }
}
