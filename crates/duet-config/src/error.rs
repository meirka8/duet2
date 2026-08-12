// SPDX-License-Identifier: MIT
//! The error taxonomy for `duet-config`.

use std::path::PathBuf;

/// Everything that can go wrong loading, migrating, or saving a config file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read from disk (missing, permission denied, …).
    #[error("failed to read {path}: {source}")]
    Read {
        /// The file that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The file could not be written to disk.
    #[error("failed to write {path}: {source}")]
    Write {
        /// The file that could not be written.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The bytes on disk are not valid TOML.
    ///
    /// Carries the location the way `docs/config-schema.md`'s hot-reload
    /// section requires ("a non-blocking diagnostic (file, line, column) is
    /// surfaced"): [`toml_edit::TomlError::span`] is preserved and turned
    /// into a 1-based line/column pair against the source text.
    #[error("failed to parse {path} at line {line}, column {column}: {message}")]
    Parse {
        /// The file that failed to parse.
        path: PathBuf,
        /// 1-based line number of the parse failure, if known.
        line: usize,
        /// 1-based column number of the parse failure, if known.
        column: usize,
        /// The underlying parser message.
        message: String,
    },

    /// The document parsed as TOML but did not match the expected typed
    /// shape (e.g. a field with the wrong type).
    #[error("failed to interpret {path} as its expected schema: {source}")]
    Deserialize {
        /// The file whose contents didn't match the schema.
        path: PathBuf,
        /// The underlying deserialization error.
        #[source]
        source: toml_edit::de::Error,
    },

    /// A migration step declared a `(from_version, to_version)` pair that
    /// doesn't chain from the file's current `schema_version`, or no
    /// registered migration covers the gap. This is a programming error in
    /// the migration registry, not a user-facing data problem.
    #[error(
        "no migration registered from schema_version {from} (needed to reach {target}) for {path}"
    )]
    NoMigrationPath {
        /// The file that couldn't be migrated.
        path: PathBuf,
        /// The version the file is currently at.
        from: u32,
        /// The version the caller wanted to reach.
        target: u32,
    },

    /// A backup could not be written before a migration ran. Migrations
    /// never mutate the on-disk file without a backup landing first (§
    /// "Schema versioning and migration" in `docs/config-schema.md`).
    #[error("failed to write migration backup {path}: {source}")]
    Backup {
        /// The backup path that failed to write.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The watcher thread could not be started.
    #[error("failed to start config file watcher for {path}: {source}")]
    Watch {
        /// The path that could not be watched.
        path: PathBuf,
        /// The underlying `notify` error.
        #[source]
        source: notify::Error,
    },

    /// The XDG config directory could not be resolved (no `$HOME` and no
    /// `$XDG_CONFIG_HOME`).
    #[error(
        "could not resolve the XDG config directory: neither $XDG_CONFIG_HOME nor $HOME is set"
    )]
    NoConfigDir,

    /// A miscellaneous OS-level failure with no more specific variant above
    /// (e.g. the watcher's background thread failed to spawn).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience alias used throughout this crate.
pub type Result<T> = std::result::Result<T, ConfigError>;
