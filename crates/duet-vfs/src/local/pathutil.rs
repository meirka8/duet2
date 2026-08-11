//! Path/error plumbing shared by every `local` submodule: `VPath` <-> real
//! OS path conversion, and classifying `rustix` errors into `VfsError`.
//!
//! Grows incrementally as later `T-3.1.x` submodules need more (temp
//! -sibling naming for T-3.1.4, xattr helpers for T-3.1.5, ...) rather than
//! speculatively up front, so nothing here sits unused/untested between
//! commits.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

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

/// Classifies a `std::io::Error` into a `VfsError` with a path attached and
/// the syscall name in the message, mirroring [`rustix_err`] for the
/// (fewer, in `local`) call sites that go through `std::fs`/`std::io`
/// rather than a raw `rustix` call directly.
pub fn io_err(op: &str, path: &VPath, err: std::io::Error) -> Box<VfsError> {
    let kind = ErrorKind::from_io_error(&err);
    Box::new(
        VfsError::new(kind, format!("{op}: {err}"))
            .with_path(path.clone())
            .with_source(err),
    )
}

/// Splits an absolute [`Path`] into `(parent, file_name)`. `None` for the
/// root path or any path with no file-name component — every `LocalFs`
/// mutation needs a parent to operate `*at`-relative to (T-3.1.3's
/// no-path-re-resolution discipline).
pub fn split_parent(path: &Path) -> Option<(&Path, &OsStr)> {
    let parent = path.parent()?;
    let name = path.file_name()?;
    Some((parent, name))
}

/// A short, collision-avoidant suffix for `.duet-partial-<rand>-<name>`
/// sibling files (T-3.1.4). Not cryptographically strong -- a colliding
/// name just fails the `O_EXCL` create it's used with (see
/// `local::rw::open_write`), so weakness here is not a safety hole, only a
/// (vanishingly unlikely) retry cost that isn't even implemented because
/// it's never been observed to matter.
fn random_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tid = format!("{:?}", std::thread::current().id());
    let pid = std::process::id();
    // FNV-1a-ish mix -- collision-avoidance only, not a cryptographic
    // requirement (see doc comment above).
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in nanos
        .to_le_bytes()
        .iter()
        .chain(tid.as_bytes())
        .chain(pid.to_le_bytes().iter())
    {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    format!("{h:016x}")
}

/// Builds the `.duet-partial-<rand>-<name>` sibling name for
/// `dest_file_name`, per design.md §9.3's journal contract
/// (`docs/crash-safety.md`): a crash-visible marker name, distinguishable
/// at a glance from a real file, sitting next to the real destination so
/// both survive/vanish together under the same directory's durability.
pub fn partial_sibling_name(dest_file_name: &OsStr) -> std::ffi::OsString {
    let mut s = std::ffi::OsString::from(".duet-partial-");
    s.push(random_suffix());
    s.push("-");
    s.push(dest_file_name);
    s
}

/// `true` if `name` looks like one of our own `.duet-partial-*` sibling
/// files. `#[cfg(test)]`-only today (used to assert no stray partial is
/// left behind after a successful commit) -- a future stale-partial sweep
/// is the anticipated non-test consumer, not yet built, so this stays
/// test-only rather than shipping unreachable `pub` API.
#[cfg(test)]
pub fn is_partial_name(name: &str) -> bool {
    name.starts_with(".duet-partial-")
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

    #[test]
    fn io_err_classifies_and_attaches_path() {
        let vp = VPath::local(UnixPathBuf::new("/missing").unwrap());
        let err = io_err(
            "open",
            &vp,
            std::io::Error::from(std::io::ErrorKind::NotFound),
        );
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.path(), Some(&vp));
    }

    #[test]
    fn split_parent_splits_a_normal_path() {
        let (parent, name) = split_parent(Path::new("/a/b/c.txt")).unwrap();
        assert_eq!(parent, Path::new("/a/b"));
        assert_eq!(name, OsStr::new("c.txt"));
    }

    #[test]
    fn split_parent_is_none_for_root() {
        assert!(split_parent(Path::new("/")).is_none());
    }

    #[test]
    fn partial_sibling_name_embeds_the_real_name_and_marker() {
        let name = partial_sibling_name(OsStr::new("report.pdf"));
        let name = name.to_str().unwrap();
        assert!(name.starts_with(".duet-partial-"));
        assert!(name.ends_with("-report.pdf"));
        assert!(is_partial_name(name));
    }

    #[test]
    fn partial_sibling_names_are_distinct_across_calls() {
        let a = partial_sibling_name(OsStr::new("x"));
        let b = partial_sibling_name(OsStr::new("x"));
        assert_ne!(a, b);
    }

    #[test]
    fn is_partial_name_rejects_ordinary_names() {
        assert!(!is_partial_name("report.pdf"));
        assert!(!is_partial_name(".hidden"));
    }
}
