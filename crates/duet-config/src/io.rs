// SPDX-License-Identifier: MIT
//! Crash-safe file I/O primitives shared by every config-file kind.
//!
//! A config save must never leave `settings.toml` half-written if the
//! process is killed mid-write (NFR-08's data-safety posture applies here
//! too, even though the blast radius of a corrupted *config* file is much
//! smaller than user data). [`atomic_write`] gets there by:
//!
//! 1. Opening the parent directory once, and doing every subsequent
//!    filesystem operation `*at`-relative to that one directory file
//!    descriptor (`openat`, `renameat2`) rather than re-resolving the full
//!    path each time.
//! 2. Writing the new content to a sibling temp file, then `fsync`-ing it
//!    (data reaches disk before anything references it by its final name).
//! 3. `renameat2`-ing the temp file over the target. `rename(2)` within a
//!    single directory is atomic on Linux: a reader always sees either the
//!    fully-old or fully-new file, never a partial write.
//! 4. `fsync`-ing the directory itself, so the rename's directory-entry
//!    update is durable too (without this, a crash right after the rename
//!    can -- on some filesystems -- lose the rename despite the file
//!    content itself being safely on disk).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use rustix::fd::AsFd;
use rustix::fs::{Mode, OFlags, RenameFlags};

use crate::error::{ConfigError, Result};

/// Reads a file to a `String`, mapping I/O failures to [`ConfigError::Read`].
pub fn read_to_string(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })
}

/// Atomically overwrites (or creates) `path` with `contents`.
///
/// See the module docs for the write-temp / fsync / renameat2 / fsync-dir
/// sequence this performs. `path`'s parent directory must already exist.
pub fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    })?;

    let file_name = path.file_name().ok_or_else(|| ConfigError::Write {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "config path has no file name",
        ),
    })?;
    let tmp_name = tmp_file_name(file_name);

    let write_err = |source: std::io::Error| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    };

    let dir_fd = rustix::fs::open(parent, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())
        .map_err(|e| write_err(e.into()))?;

    // Write + fsync the temp file, relative to the open directory fd.
    {
        let tmp_fd = rustix::fs::openat(
            &dir_fd,
            &tmp_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC,
            Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH,
        )
        .map_err(|e| write_err(e.into()))?;
        let mut file = std::fs::File::from(tmp_fd);
        file.write_all(contents.as_bytes()).map_err(write_err)?;
        file.sync_all().map_err(write_err)?;
    }

    // Atomically replace the target with the temp file, then make the
    // directory-entry update itself durable.
    rustix::fs::renameat_with(&dir_fd, &tmp_name, &dir_fd, file_name, RenameFlags::empty())
        .map_err(|e| write_err(e.into()))?;
    rustix::fs::fsync(dir_fd.as_fd()).map_err(|e| write_err(e.into()))?;

    Ok(())
}

/// Writes `contents` verbatim to a new backup file, fsync'd for durability,
/// per `docs/config-schema.md`'s migration backup naming
/// (`<file>.v<N>.bak-<unix_ts>`).
pub fn write_backup(backup_path: &Path, contents: &str) -> Result<()> {
    let mut file = std::fs::File::create(backup_path).map_err(|source| ConfigError::Backup {
        path: backup_path.to_path_buf(),
        source,
    })?;
    file.write_all(contents.as_bytes())
        .map_err(|source| ConfigError::Backup {
            path: backup_path.to_path_buf(),
            source,
        })?;
    file.sync_all().map_err(|source| ConfigError::Backup {
        path: backup_path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Builds `<original>.v<schema_version>.bak-<unix_ts>` next to `path`, per
/// `docs/config-schema.md`'s migration section.
pub fn backup_path(path: &Path, from_version: u32, unix_ts: u64) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.toml");
    path.with_file_name(format!("{file_name}.v{from_version}.bak-{unix_ts}"))
}

fn tmp_file_name(file_name: &std::ffi::OsStr) -> std::ffi::OsString {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut name = file_name.to_os_string();
    name.push(format!(".tmp-{pid}-{nanos}"));
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        atomic_write(&path, "schema_version = 1\n").unwrap();
        assert_eq!(read_to_string(&path).unwrap(), "schema_version = 1\n");

        // Overwrite: no leftover temp files, content fully replaced.
        atomic_write(&path, "schema_version = 2\n").unwrap();
        assert_eq!(read_to_string(&path).unwrap(), "schema_version = 2\n");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp file(s) leaked: {leftovers:?}");
    }

    #[test]
    fn backup_path_matches_documented_naming() {
        let p = backup_path(
            Path::new("/home/x/.config/duet/settings.toml"),
            0,
            1_700_000_000,
        );
        assert_eq!(
            p,
            PathBuf::from("/home/x/.config/duet/settings.toml.v0.bak-1700000000")
        );
    }
}
