//! Path/error plumbing shared by every `local` submodule: `VPath` <-> real
//! OS path conversion, and classifying `rustix` errors into `VfsError`.
//!
//! Grows incrementally as later `T-3.1.x` submodules need more (temp
//! -sibling naming for T-3.1.4, xattr helpers for T-3.1.5, ...) rather than
//! speculatively up front, so nothing here sits unused/untested between
//! commits.

use std::path::PathBuf;

use duet_types::{ErrorKind, VPath, VfsError};

/// Converts a local-mount `VPath` to the real absolute OS path it addresses.
///
/// `LocalFs` only ever mounts `file:` root mounts (no authority, never
/// nested — a nested mount atop `file:` would be handled by the `archive`/
/// `remote` backends layering on top of this one, not by `LocalFs` itself),
/// so this is a direct, lossless textual mapping: `UnixPathBuf` is already
/// `/`-separated absolute text, which is exactly what a `Path` on Linux is.
pub fn real_path(p: &VPath) -> PathBuf {
    PathBuf::from(p.inner().as_str())
}

/// Classifies a `rustix::io::Errno` into a `VfsError`, attaching `path` and
/// a human-readable message that includes which syscall failed (so error
/// text in logs/the conflict UI says *what* failed, not just *that* it
/// did).
pub fn rustix_err(op: &str, path: &VPath, err: rustix::io::Errno) -> Box<VfsError> {
    let kind = ErrorKind::from_errno(err.raw_os_error());
    Box::new(
        VfsError::new(kind, format!("{op}: {err}"))
            .with_path(path.clone())
            .with_source(std::io::Error::from(err)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use duet_types::UnixPathBuf;

    #[test]
    fn real_path_maps_local_vpath_directly() {
        let vp = VPath::local(UnixPathBuf::new("/a/b/c").unwrap());
        assert_eq!(real_path(&vp), PathBuf::from("/a/b/c"));
    }

    #[test]
    fn rustix_err_classifies_and_attaches_path() {
        let vp = VPath::local(UnixPathBuf::new("/missing").unwrap());
        let err = rustix_err("openat", &vp, rustix::io::Errno::NOENT);
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.path(), Some(&vp));
    }
}
