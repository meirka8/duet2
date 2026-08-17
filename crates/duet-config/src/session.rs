// SPDX-License-Identifier: MIT
//! `~/.local/state/duet/session.json` -- panes, tabs, and cwds (T-4.3.2's
//! slice of design.md §10's session file; cursor position, sort, view mode,
//! and the splitter ratio are T-4.3.7's job, added to this same file
//! later). Plain `serde_json`, not `duet_config::document`'s round-trip
//! `toml_edit` machinery: this file is app state a user never hand-edits,
//! so there's nothing to preserve across a save the way a config file's
//! comments/formatting/unknown-keys need to be.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};
use crate::io;

/// Bumped whenever [`Session`]'s shape changes incompatibly. No migration
/// chain exists yet (unlike the TOML config files' [`crate::migrate`]) --
/// there is only one version so far, and a session file, unlike a config
/// file, is disposable: a version this build doesn't understand is treated
/// as absent (see [`load`]'s doc comment) rather than migrated, since
/// losing a session's tab layout is a minor inconvenience, not a data-loss
/// event worth a backup-and-migrate pipeline over.
pub const SESSION_SCHEMA_VERSION: u32 = 1;

/// Which column a tab was last sorted by, for [`SessionTab::sort_column`].
/// Mirrors `duet_index::SortColumn`'s Name/Size/Modified variants (the
/// only three `duet-ui`'s `FileTableDelegate` ever actually sorts by --
/// `Kind` is a real `SortColumn` variant but isn't reachable through any
/// column this app renders) without this crate depending on `duet-index`
/// just for a persistence enum; `duet-ui` translates between the two
/// explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SessionSortColumn {
    #[default]
    Name,
    Size,
    Modified,
}

fn default_true() -> bool {
    true
}

/// One tab's persisted state: which directory it's showing, its two TC
/// lock flags (T-4.3.2), and its cursor position + sort state (T-4.3.7).
/// View mode does *not* appear here -- T-4.2.5 (Full/Brief/Thumbnails/
/// Tree) was never implemented, so there is nothing to persist; add it
/// when that task lands, not speculatively ahead of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTab {
    pub dir: PathBuf,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub lock_dir_change: bool,
    /// The name of the entry under the cursor when last saved. `None` for
    /// an empty listing, the synthetic ".." row, or a tab that's never had
    /// its cursor position observed. `#[serde(default)]` so a
    /// `session.json` written by T-4.3.2 (before this field existed)
    /// still loads. Restored by name, not row index -- directory contents
    /// can change between runs; a name that no longer exists on restore
    /// just falls back to row 0, the same graceful-miss behavior
    /// `FileTableDelegate::select_row_by_name` already has for T-4.3.1's
    /// "cursor restores to the child directory when going up".
    #[serde(default)]
    pub cursor_name: Option<String>,
    /// Sort column + direction (T-4.3.7). Both default (missing key =
    /// `Name` ascending, `duet_index::SortOptions::default()`'s own
    /// answer) for the same backward-compatibility reason as
    /// `cursor_name`.
    #[serde(default)]
    pub sort_column: SessionSortColumn,
    #[serde(default = "default_true")]
    pub sort_ascending: bool,
}

/// One panel's persisted tab list plus which tab was active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPanel {
    pub tabs: Vec<SessionTab>,
    #[serde(default)]
    pub active_tab: usize,
}

/// The full `session.json` document: both panels, each with their own tabs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub schema_version: u32,
    pub left: SessionPanel,
    pub right: SessionPanel,
}

/// Loads `session.json` from `path`.
///
/// Any failure -- the file doesn't exist yet (first launch), isn't valid
/// JSON, doesn't match [`Session`]'s shape, or names a future
/// `schema_version` this build predates -- is reported as
/// [`ConfigError::SessionParse`]/[`ConfigError::Read`] rather than crashing
/// or discarding anything: session state is the kind of thing a fresh
/// default degrades to gracefully (matching T-4.3.7's own explicit AC, "a
/// corrupt session file degrades to defaults with a notice", applied here
/// a task early since there's no reason to wait). Callers should log the
/// error and fall back to a single default tab per panel, never propagate
/// it as a startup failure.
pub fn load(path: &Path) -> Result<Session> {
    let text = io::read_to_string(path)?;
    let session: Session =
        serde_json::from_str(&text).map_err(|source| ConfigError::SessionParse {
            path: path.to_path_buf(),
            source,
        })?;
    if session.schema_version > SESSION_SCHEMA_VERSION {
        return Err(ConfigError::SessionParse {
            path: path.to_path_buf(),
            source: serde::de::Error::custom(format!(
                "session schema_version {} is newer than this build supports ({})",
                session.schema_version, SESSION_SCHEMA_VERSION
            )),
        });
    }
    Ok(session)
}

/// Writes `session` to `path` atomically (via [`io::atomic_write`], the
/// same write-temp/fsync/renameat2/fsync-dir sequence every other config
/// kind in this crate uses -- NFR-08's data-safety posture applies to
/// "don't corrupt session.json on a kill -9 mid-write" just as much as it
/// does to settings.toml, even though losing it is lower-stakes than
/// losing user data).
pub fn save(path: &Path, session: &Session) -> Result<()> {
    let text =
        serde_json::to_string_pretty(session).map_err(|source| ConfigError::SessionParse {
            path: path.to_path_buf(),
            source,
        })?;
    io::atomic_write(path, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Session {
        Session {
            schema_version: SESSION_SCHEMA_VERSION,
            left: SessionPanel {
                tabs: vec![
                    SessionTab {
                        dir: PathBuf::from("/home/user"),
                        locked: false,
                        lock_dir_change: false,
                        cursor_name: Some("Documents".to_string()),
                        sort_column: SessionSortColumn::Modified,
                        sort_ascending: false,
                    },
                    SessionTab {
                        dir: PathBuf::from("/home/user/projects"),
                        locked: true,
                        lock_dir_change: false,
                        cursor_name: None,
                        sort_column: SessionSortColumn::Name,
                        sort_ascending: true,
                    },
                ],
                active_tab: 1,
            },
            right: SessionPanel {
                tabs: vec![SessionTab {
                    dir: PathBuf::from("/tmp"),
                    locked: false,
                    lock_dir_change: true,
                    cursor_name: Some("scratch.txt".to_string()),
                    sort_column: SessionSortColumn::Size,
                    sort_ascending: true,
                }],
                active_tab: 0,
            },
        }
    }

    #[test]
    fn round_trips_through_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let session = sample();

        save(&path, &session).unwrap();
        let loaded = load(&path).unwrap();

        assert_eq!(loaded, session);
    }

    #[test]
    fn load_of_missing_file_is_a_read_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        assert!(matches!(load(&path), Err(ConfigError::Read { .. })));
    }

    #[test]
    fn load_of_garbage_bytes_is_a_session_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(&path, "not json at all { [ }").unwrap();
        assert!(matches!(load(&path), Err(ConfigError::SessionParse { .. })));
    }

    #[test]
    fn load_of_a_future_schema_version_is_rejected_not_silently_misread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(
            &path,
            r#"{"schema_version":9999,"left":{"tabs":[],"active_tab":0},"right":{"tabs":[],"active_tab":0}}"#,
        )
        .unwrap();
        assert!(matches!(load(&path), Err(ConfigError::SessionParse { .. })));
    }

    #[test]
    fn missing_lock_flags_default_to_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(
            &path,
            r#"{"schema_version":1,"left":{"tabs":[{"dir":"/a"}],"active_tab":0},"right":{"tabs":[],"active_tab":0}}"#,
        )
        .unwrap();
        let session = load(&path).unwrap();
        assert!(!session.left.tabs[0].locked);
        assert!(!session.left.tabs[0].lock_dir_change);
    }

    /// T-4.3.7: a `session.json` written by T-4.3.2 (before cursor_name/
    /// sort_column/sort_ascending existed) must still load, with those
    /// three fields defaulting to "no cursor position recorded, Name
    /// ascending" -- the same answer a fresh listing starts at anyway, so
    /// an old session file degrades to indistinguishable-from-normal, not
    /// broken.
    #[test]
    fn missing_cursor_and_sort_fields_default_to_name_ascending_no_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(
            &path,
            r#"{"schema_version":1,"left":{"tabs":[{"dir":"/a"}],"active_tab":0},"right":{"tabs":[],"active_tab":0}}"#,
        )
        .unwrap();
        let session = load(&path).unwrap();
        assert_eq!(session.left.tabs[0].cursor_name, None);
        assert_eq!(session.left.tabs[0].sort_column, SessionSortColumn::Name);
        assert!(session.left.tabs[0].sort_ascending);
    }

    #[test]
    fn saving_twice_leaves_no_leftover_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        save(&path, &sample()).unwrap();
        save(&path, &sample()).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp file(s) leaked: {leftovers:?}");
    }
}
