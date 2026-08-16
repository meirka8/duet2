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
