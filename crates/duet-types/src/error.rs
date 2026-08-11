//! The error taxonomy (design.md §9.3).
//!
//! Every failure in the VFS/ops stack is classified into one of six
//! buckets so `duet-ops`'s planner/executor can react mechanically rather
//! than string-matching:
//!
//! - [`ErrorKind::Retryable`] — EINTR, EAGAIN, transient network blips.
//!   Gets bounded exponential backoff (T-5.1.10).
//! - [`ErrorKind::Permission`] — EACCES/EPERM. Offers FR-OPS-13 elevation.
//! - [`ErrorKind::Space`] — ENOSPC/EDQUOT. Pauses the *whole queue*, not
//!   just the job.
//! - [`ErrorKind::NotFound`] — the target vanished (race with another
//!   actor, or a stale listing).
//! - [`ErrorKind::Conflict`] — destination already exists, non-empty
//!   directory on remove, etc. Feeds the conflict-resolution UI.
//! - [`ErrorKind::Fatal`] — anything else; ends the job `Failed`.
//!
//! [`VfsError`] is the concrete error type carrying a classified kind plus
//! context (which `VPath`, and the underlying `io::Error`/source, if any).
//! [`Result`] is the crate-wide alias `design.md` §9.1's `FileSystem` trait
//! sketch uses bare (`async fn stat(...) -> Result<Metadata>`).

use std::fmt;

use crate::path::VPath;

/// Linux errno values duet-types classifies by. Hand-rolled (not `libc`)
/// to keep this crate at "no deps beyond std/serde" (design.md §8.1);
/// Duet is Linux-only so these are stable ABI constants, not guesses.
pub mod errno {
    pub const EPERM: i32 = 1;
    pub const ENOENT: i32 = 2;
    pub const EINTR: i32 = 4;
    pub const EIO: i32 = 5;
    pub const EAGAIN: i32 = 11;
    pub const EACCES: i32 = 13;
    pub const EBUSY: i32 = 16;
    pub const EEXIST: i32 = 17;
    pub const EXDEV: i32 = 18;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
    pub const ENOTEMPTY: i32 = 39;
    pub const ELOOP: i32 = 40;
    pub const ENAMETOOLONG: i32 = 36;
    pub const EROFS: i32 = 30;
    pub const ENOSPC: i32 = 28;
    pub const EDQUOT: i32 = 122;
    pub const ECONNRESET: i32 = 104;
    pub const ETIMEDOUT: i32 = 110;
    pub const ECONNREFUSED: i32 = 111;
    pub const EHOSTUNREACH: i32 = 113;
}

/// The six-way classification every VFS/ops failure is bucketed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ErrorKind {
    /// Transient: EINTR, EAGAIN, a dropped connection mid-request. Safe to
    /// retry with backoff.
    #[error("retryable")]
    Retryable,
    /// EACCES/EPERM. The engine may offer to retry elevated (FR-OPS-13).
    #[error("permission denied")]
    Permission,
    /// ENOSPC/EDQUOT. Distinct from `Fatal` because the *queue*, not just
    /// the job, must pause until the operator frees space.
    #[error("out of space or quota exceeded")]
    Space,
    /// The target path does not exist (or vanished mid-operation).
    #[error("not found")]
    NotFound,
    /// Destination exists, directory not empty, etc. — feeds the
    /// conflict-resolution prompt rather than failing the job outright.
    #[error("conflict")]
    Conflict,
    /// Anything not covered above. Ends the job `Failed`.
    #[error("fatal error")]
    Fatal,
}

impl ErrorKind {
    /// Classifies a raw Linux errno per design.md §9.3's taxonomy.
    pub fn from_errno(errno: i32) -> ErrorKind {
        use errno::*;
        match errno {
            EINTR | EAGAIN | EBUSY | ECONNRESET | ETIMEDOUT | ECONNREFUSED | EHOSTUNREACH => {
                ErrorKind::Retryable
            }
            EACCES | EPERM | EROFS => ErrorKind::Permission,
            ENOSPC | EDQUOT => ErrorKind::Space,
            ENOENT => ErrorKind::NotFound,
            EEXIST | ENOTEMPTY | ENOTDIR | EISDIR => ErrorKind::Conflict,
            _ => ErrorKind::Fatal,
        }
    }

    /// Classifies a `std::io::Error`: prefers the precise raw errno when
    /// the OS supplied one, falling back to `std::io::ErrorKind` for
    /// synthetic errors (e.g. constructed by a non-local backend that has
    /// no raw errno to report).
    pub fn from_io_error(err: &std::io::Error) -> ErrorKind {
        if let Some(errno) = err.raw_os_error() {
            return ErrorKind::from_errno(errno);
        }
        use std::io::ErrorKind as IoKind;
        match err.kind() {
            IoKind::NotFound => ErrorKind::NotFound,
            IoKind::PermissionDenied => ErrorKind::Permission,
            IoKind::AlreadyExists => ErrorKind::Conflict,
            IoKind::WouldBlock | IoKind::Interrupted | IoKind::TimedOut => ErrorKind::Retryable,
            _ => ErrorKind::Fatal,
        }
    }

    pub fn is_retryable(self) -> bool {
        self == ErrorKind::Retryable
    }
}

/// A classified, context-carrying error from the VFS/ops stack.
///
/// Carries the [`ErrorKind`] bucket, the `VPath` the failure happened on
/// (when known), a human-readable message, and the underlying error (an
/// I/O error, a backend-specific error, etc.) as `source()`.
#[derive(Debug)]
pub struct VfsError {
    kind: ErrorKind,
    message: String,
    path: Option<VPath>,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl VfsError {
    /// Constructs an error directly from a classification and message,
    /// with no path context or source.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        VfsError {
            kind,
            message: message.into(),
            path: None,
            source: None,
        }
    }

    /// Constructs a `VfsError` from a `std::io::Error`, classifying it via
    /// [`ErrorKind::from_io_error`].
    pub fn from_io(err: std::io::Error) -> Self {
        let kind = ErrorKind::from_io_error(&err);
        let message = err.to_string();
        VfsError {
            kind,
            message,
            path: None,
            source: Some(Box::new(err)),
        }
    }

    /// Attaches the `VPath` this error occurred on.
    pub fn with_path(mut self, path: VPath) -> Self {
        self.path = Some(path);
        self
    }

    /// Attaches an arbitrary source error.
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn path(&self) -> Option<&VPath> {
        self.path.as_ref()
    }

    pub fn is_retryable(&self) -> bool {
        self.kind.is_retryable()
    }
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)?;
        if let Some(path) = &self.path {
            write!(f, " ({path})")?;
        }
        Ok(())
    }
}

impl std::error::Error for VfsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e.as_ref() as &(dyn std::error::Error + 'static))
    }
}

impl From<std::io::Error> for VfsError {
    fn from(err: std::io::Error) -> Self {
        VfsError::from_io(err)
    }
}

/// The crate-wide `Result` alias used throughout the VFS/ops stack (see
/// design.md §9.1's `FileSystem` trait, which uses bare `Result<T>`).
///
/// The error is boxed (`Box<VfsError>`, not `VfsError` directly) because
/// `VfsError` carries a `VPath` plus an optional source error and comes out
/// to ~144 bytes — large enough that clippy's `result_large_err` correctly
/// flags every `FileSystem` method paying that cost in the stack size of its
/// `Result` even on the (far more common) `Ok` path. `?` continues to work
/// unchanged: `Box<T>: From<T>` is a std blanket impl.
pub type Result<T, E = Box<VfsError>> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_errnos() {
        assert_eq!(ErrorKind::from_errno(errno::ENOSPC), ErrorKind::Space);
        assert_eq!(ErrorKind::from_errno(errno::EDQUOT), ErrorKind::Space);
        assert_eq!(ErrorKind::from_errno(errno::EACCES), ErrorKind::Permission);
        assert_eq!(ErrorKind::from_errno(errno::ENOENT), ErrorKind::NotFound);
        assert_eq!(ErrorKind::from_errno(errno::EEXIST), ErrorKind::Conflict);
        assert_eq!(ErrorKind::from_errno(errno::EINTR), ErrorKind::Retryable);
        assert_eq!(ErrorKind::from_errno(9999), ErrorKind::Fatal);
    }

    #[test]
    fn io_error_round_trip_classification() {
        use std::error::Error as _;
        let err = std::io::Error::from_raw_os_error(errno::ENOSPC);
        let vfs_err = VfsError::from_io(err);
        assert_eq!(vfs_err.kind(), ErrorKind::Space);
        assert!(vfs_err.source().is_some());
    }

    #[test]
    fn display_includes_path_when_present() {
        let err = VfsError::new(ErrorKind::NotFound, "missing")
            .with_path(VPath::local(crate::path::UnixPathBuf::new("/a/b").unwrap()));
        let s = err.to_string();
        assert!(s.contains("not found"));
        assert!(s.contains("file:///a/b"));
    }
}
