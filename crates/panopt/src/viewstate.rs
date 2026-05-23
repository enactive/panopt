//! Local, per-cockpit view state for the viewer panes.
//!
//! A viewer remembers where you were in each item it has shown - the scroll
//! offset of a long scratchpad, the cursor row of a list - so leaving an item
//! and returning lands in the same place. This is a personal preference, not
//! coordination state, so it lives on the cockpit side under the user's config
//! directory, never in the daemon or the shared `.panopt/` projection.
//!
//! One JSON file per project, keyed by item (`todo:3`, `list:todos`, ...).
//! Reads and writes go through the file each time, so several viewer panes can
//! persist their own items without clobbering each other's keys.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// A remembered position within one item.
#[derive(Clone, Copy, Default)]
pub struct ViewState {
    /// First visible row, for a scrollable document.
    pub scroll: u16,
    /// Selected row, for a navigable list.
    pub cursor: usize,
}

/// A stable hex hash of a project path, used as its view-state filename.
///
/// FNV-1a, hand-rolled so the name never shifts under a compiler upgrade - a
/// changed name would silently forget every remembered position.
fn path_hash(project: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in project.to_string_lossy().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The view-state file for `project`, its parent directory created.
fn store_path(project: &Path) -> Option<PathBuf> {
    let dir = dirs::config_dir()?.join("panopt").join("viewstate");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join(format!("{}.json", path_hash(project))))
}

/// The project's stored map, or an empty one when the file is absent or junk.
fn read(project: &Path) -> Map<String, Value> {
    let Some(path) = store_path(project) else {
        return Map::new();
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Write `map` to the project's file via a temp file and rename.
fn write(project: &Path, map: &Map<String, Value>) {
    let Some(path) = store_path(project) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, Value::Object(map.clone()).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// The remembered state for `key`, or a default when none is stored.
pub fn get(project: &Path, key: &str) -> ViewState {
    match read(project).get(key) {
        Some(v) => ViewState {
            scroll: v.get("scroll").and_then(Value::as_u64).unwrap_or(0) as u16,
            cursor: v.get("cursor").and_then(Value::as_u64).unwrap_or(0) as usize,
        },
        None => ViewState::default(),
    }
}

/// Persist the state for `key`, read-modify-writing the project's file so a
/// concurrent viewer pane does not clobber another key.
pub fn set(project: &Path, key: &str, state: ViewState) {
    let mut map = read(project);
    map.insert(
        key.to_string(),
        json!({ "scroll": state.scroll, "cursor": state.cursor }),
    );
    write(project, &map);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_hash_is_stable_and_path_specific() {
        let a = path_hash(Path::new("/Users/x/p/one"));
        assert_eq!(a, path_hash(Path::new("/Users/x/p/one")));
        assert_ne!(a, path_hash(Path::new("/Users/x/p/two")));
        assert_eq!(a.len(), 16);
    }
}
