// SPDX-License-Identifier: MIT
//! XDG path resolution for the four config-file kinds documented in
//! `docs/config-schema.md` §0 ("Directory layout").
//!
//! Deliberately hand-rolled rather than pulling in an `xdg`/`dirs` crate:
//! the subset of the spec Duet needs is three lines (`$XDG_CONFIG_HOME` or
//! `$HOME/.config`), and design.md §7.5's dependency policy asks that
//! additions earn their keep.

use std::path::PathBuf;

use crate::error::{ConfigError, Result};

/// Resolves `$XDG_CONFIG_HOME`, falling back to `$HOME/.config` per the XDG
/// Base Directory Specification.
///
/// Returns [`ConfigError::NoConfigDir`] if neither environment variable is
/// set (an unusual but possible situation, e.g. a stripped-down container).
pub fn xdg_config_home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home).join(".config"));
    }
    Err(ConfigError::NoConfigDir)
}

/// `~/.config/duet` (or `$XDG_CONFIG_HOME/duet`).
pub fn duet_config_dir() -> Result<PathBuf> {
    Ok(xdg_config_home()?.join("duet"))
}

/// Resolves `$XDG_STATE_HOME`, falling back to `$HOME/.local/state` per the
/// XDG Base Directory Specification. State (`session.json`, operation
/// journals, history) is not user-editable config, so it lives in a
/// separate tree from [`xdg_config_home`] -- see design.md §10's directory
/// layout.
pub fn xdg_state_home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home).join(".local").join("state"));
    }
    Err(ConfigError::NoStateDir)
}

/// `~/.local/state/duet` (or `$XDG_STATE_HOME/duet`).
pub fn duet_state_dir() -> Result<PathBuf> {
    Ok(xdg_state_home()?.join("duet"))
}

/// Resolves `$XDG_DATA_HOME`, falling back to `$HOME/.local/share` per the
/// XDG Base Directory Specification. Data (the trash, per the freedesktop
/// trash spec) is neither user-editable config nor throwaway state, so it
/// lives in its own tree alongside [`xdg_config_home`]/[`xdg_state_home`].
pub fn xdg_data_home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home).join(".local").join("share"));
    }
    Err(ConfigError::NoDataDir)
}

/// `~/.local/share/Trash/files` (or `$XDG_DATA_HOME/Trash/files`) -- where
/// T-5.2.6's trash-mode delete moves its targets.
///
/// **A deliberately minimal placeholder, not the freedesktop trash spec.**
/// design.md §9.10/FR-CFG-07's full implementation -- `.trashinfo` sidecars
/// recording each item's original path and deletion time, `$topdir/
/// .Trash-$uid` for targets on other mounts, and a browsable/restorable
/// trash view -- is T-5.3.1's own, later scope. This function does none of
/// that; it only answers "which directory does a trashed file move into,"
/// which is all `duet_ops::DeleteMode::Trash` needs (see
/// `duet_ops::deleter`'s own module doc comment for why "trash" is just
/// `plan_move` into a directory at that layer).
///
/// Nothing built on this needs undoing or migrating when T-5.3.1 lands:
/// this is the *same* final location the real spec-compliant
/// implementation uses for a home-filesystem delete. T-5.3.1 layers the
/// sidecar metadata and the other-mount cases on top of this destination
/// rather than replacing it.
pub fn trash_files_dir() -> Result<PathBuf> {
    Ok(xdg_data_home()?.join("Trash").join("files"))
}

/// `~/.local/state/duet/session.json` -- panes, tabs, cwds (design.md §10).
pub fn session_path() -> Result<PathBuf> {
    Ok(duet_state_dir()?.join("session.json"))
}

/// `~/.config/duet/settings.toml`.
pub fn settings_path() -> Result<PathBuf> {
    Ok(duet_config_dir()?.join("settings.toml"))
}

/// `~/.config/duet/keymap.toml` (the user layer; base files live under
/// [`keymap_base_path`]).
pub fn keymap_path() -> Result<PathBuf> {
    Ok(duet_config_dir()?.join("keymap.toml"))
}

/// `~/.config/duet/keymaps/{tc,mc,modern}.toml` -- the shipped, read-only
/// base keymaps. `base` should be one of `"tc"`, `"mc"`, `"modern"`.
pub fn keymap_base_path(base: &str) -> Result<PathBuf> {
    Ok(duet_config_dir()?
        .join("keymaps")
        .join(format!("{base}.toml")))
}

/// `~/.config/duet/connections.toml`.
pub fn connections_path() -> Result<PathBuf> {
    Ok(duet_config_dir()?.join("connections.toml"))
}

/// `~/.config/duet/hotlist.toml` (T-4.3.5, FR-NAV-08).
pub fn hotlist_path() -> Result<PathBuf> {
    Ok(duet_config_dir()?.join("hotlist.toml"))
}

/// `~/.config/duet/themes/<name>.toml`.
pub fn theme_path(name: &str) -> Result<PathBuf> {
    Ok(duet_config_dir()?
        .join("themes")
        .join(format!("{name}.toml")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_config_home_prefers_explicit_var() {
        // SAFETY: test runs single-threaded w.r.t. this env var via
        // `--test-threads=1` sensitivity is avoided by scoping via a guard
        // pattern; std::env mutation in tests is inherently process-global,
        // so keep this test isolated to variables no other test touches.
        temp_env(
            &[
                ("XDG_CONFIG_HOME", Some("/tmp/xdg-explicit")),
                ("HOME", Some("/tmp/home-fallback")),
            ],
            || {
                assert_eq!(
                    xdg_config_home().unwrap(),
                    PathBuf::from("/tmp/xdg-explicit")
                );
            },
        );
    }

    #[test]
    fn xdg_config_home_falls_back_to_home() {
        temp_env(
            &[
                ("XDG_CONFIG_HOME", None),
                ("HOME", Some("/tmp/home-fallback")),
            ],
            || {
                assert_eq!(
                    xdg_config_home().unwrap(),
                    PathBuf::from("/tmp/home-fallback/.config")
                );
            },
        );
    }

    #[test]
    fn xdg_state_home_prefers_explicit_var() {
        temp_env(
            &[
                ("XDG_STATE_HOME", Some("/tmp/xdg-state-explicit")),
                ("HOME", Some("/tmp/home-fallback")),
            ],
            || {
                assert_eq!(
                    xdg_state_home().unwrap(),
                    PathBuf::from("/tmp/xdg-state-explicit")
                );
            },
        );
    }

    #[test]
    fn xdg_state_home_falls_back_to_home_dot_local_state() {
        temp_env(
            &[
                ("XDG_STATE_HOME", None),
                ("HOME", Some("/tmp/home-fallback")),
            ],
            || {
                assert_eq!(
                    xdg_state_home().unwrap(),
                    PathBuf::from("/tmp/home-fallback/.local/state")
                );
            },
        );
    }

    #[test]
    fn xdg_data_home_prefers_explicit_var() {
        temp_env(
            &[
                ("XDG_DATA_HOME", Some("/tmp/xdg-data-explicit")),
                ("HOME", Some("/tmp/home-fallback")),
            ],
            || {
                assert_eq!(
                    xdg_data_home().unwrap(),
                    PathBuf::from("/tmp/xdg-data-explicit")
                );
            },
        );
    }

    #[test]
    fn xdg_data_home_falls_back_to_home_dot_local_share() {
        temp_env(
            &[
                ("XDG_DATA_HOME", None),
                ("HOME", Some("/tmp/home-fallback")),
            ],
            || {
                assert_eq!(
                    xdg_data_home().unwrap(),
                    PathBuf::from("/tmp/home-fallback/.local/share")
                );
            },
        );
    }

    #[test]
    fn xdg_data_home_errors_when_neither_var_is_set() {
        temp_env(&[("XDG_DATA_HOME", None), ("HOME", None)], || {
            assert!(matches!(xdg_data_home(), Err(ConfigError::NoDataDir)));
        });
    }

    #[test]
    fn trash_files_dir_is_under_xdg_data_home() {
        temp_env(
            &[
                ("XDG_DATA_HOME", Some("/tmp/xdg-data-explicit")),
                ("HOME", None),
            ],
            || {
                assert_eq!(
                    trash_files_dir().unwrap(),
                    PathBuf::from("/tmp/xdg-data-explicit/Trash/files")
                );
            },
        );
    }

    #[test]
    fn session_path_appends_duet_session_json() {
        temp_env(
            &[
                ("XDG_STATE_HOME", Some("/tmp/xdg-state-explicit")),
                ("HOME", None),
            ],
            || {
                assert_eq!(
                    session_path().unwrap(),
                    PathBuf::from("/tmp/xdg-state-explicit/duet/session.json")
                );
            },
        );
    }

    #[test]
    fn hotlist_path_appends_duet_hotlist_toml() {
        temp_env(
            &[
                ("XDG_CONFIG_HOME", Some("/tmp/xdg-explicit")),
                ("HOME", None),
            ],
            || {
                assert_eq!(
                    hotlist_path().unwrap(),
                    PathBuf::from("/tmp/xdg-explicit/duet/hotlist.toml")
                );
            },
        );
    }

    #[test]
    fn duet_config_dir_appends_duet() {
        temp_env(
            &[
                ("XDG_CONFIG_HOME", Some("/tmp/xdg-explicit")),
                ("HOME", None),
            ],
            || {
                assert_eq!(
                    duet_config_dir().unwrap(),
                    PathBuf::from("/tmp/xdg-explicit/duet")
                );
            },
        );
    }

    /// Runs `f` with the given environment variables set (or removed, for
    /// `None`), restoring the previous values afterward. Serialized via a
    /// process-wide mutex since env vars are process state.
    fn temp_env(vars: &[(&str, Option<&str>)], f: impl FnOnce()) {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let previous: Vec<(&str, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var(k).ok()))
            .collect();

        for (k, v) in vars {
            match v {
                // SAFETY: serialized by ENV_LOCK above; no other thread in
                // this test binary reads these specific vars concurrently.
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }

        f();

        for (k, v) in previous {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }
}
