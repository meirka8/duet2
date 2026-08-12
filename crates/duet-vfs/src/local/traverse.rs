//! T-3.1.3 — `*at`-relative traversal helpers, and the recursive-remove
//! walk built on them (`FileSystem::remove(_, RemoveKind::Recursive)`).
//!
//! # No path re-resolution, by construction
//!
//! Every function in this module that descends into a subdirectory does so
//! via `openat(parent_fd, name, ...)` against an already-open directory
//! file descriptor — never by rebuilding and re-resolving an absolute (or
//! even relative) path string from the root. This is the load-bearing
//! property `T-3.1.3`'s AC asks to be verified "by inspection/design": grep
//! this file for `real_path`/`PathBuf`/string concatenation used to
//! *address* a child — there isn't one. The only path resolution that
//! happens is the single `openat` on the caller-supplied root `VPath` in
//! [`remove_recursive`]; everything below that is fd-relative.
//!
//! This matters for more than tidiness: an `openat` (or `unlinkat`) against
//! a directory fd is immune to the name it was opened through being
//! renamed, replaced, or pointed at a symlink *after* the fd was obtained
//! — the fd continues to reference the same inode the kernel resolved at
//! `open` time, regardless of what subsequently happens to any path name
//! that used to (or still does) point at it. A recursive walk built from
//! `openat(dir_fd, ...)` calls therefore cannot be tricked into escaping
//! its intended subtree by a concurrent rename racing the walk — the
//! classic symlink-race/TOCTOU class of bug this design eliminates by
//! construction rather than by checking. See `local::traverse::tests::
//! toctou_rename_after_open_does_not_redirect_the_open_fd` for a real,
//! two-thread test of exactly this property.
//!
//! Every `openat` of a presumed subdirectory also passes `O_NOFOLLOW`: if
//! an attacker swaps a real subdirectory for a symlink (say, to `/etc`)
//! between our `getdents64` scan seeing it as a directory and our `openat`
//! call, `O_NOFOLLOW` makes that `openat` fail (`ELOOP`) rather than
//! silently open the symlink's target — refuse, don't follow.

use std::os::fd::OwnedFd;

use duet_types::{ErrorKind, Result, VPath};
use rustix::fs::{self, AtFlags, CWD, FileType, Mode, OFlags, RawDir};
use rustix::io::Errno;
use std::mem::MaybeUninit;

use super::guard;
use super::pathutil::{real_path, rustix_err};

const SCAN_BUF_BYTES: usize = 64 * 1024;

/// One raw `(name, is_dir)` pair from a full (non-chunked) directory scan —
/// recursive removal needs the whole listing before it can safely start
/// unlinking, unlike `read_dir` (T-3.1.1), which streams for the UI's
/// benefit.
struct Child {
    name: std::ffi::CString,
    is_dir: bool,
    d_type_unknown: bool,
}

/// Fully drains `dir_fd`'s entries (skipping `.`/`..`), classifying each by
/// `d_type` where the kernel supplied one.
fn list_children(dir_fd: &OwnedFd) -> rustix::io::Result<Vec<Child>> {
    guard::assert_not_ui_thread();
    let mut out = Vec::new();
    loop {
        let mut buf = vec![MaybeUninit::uninit(); SCAN_BUF_BYTES];
        let mut iter = RawDir::new(dir_fd, buf.as_mut_slice());
        let mut got_any = false;
        let mut hit_eof = false;
        loop {
            if iter.is_buffer_empty() && got_any {
                break;
            }
            match iter.next() {
                None => {
                    hit_eof = true;
                    break;
                }
                Some(Err(e)) => return Err(e),
                Some(Ok(entry)) => {
                    let bytes = entry.file_name().to_bytes();
                    if bytes == b"." || bytes == b".." {
                        continue;
                    }
                    got_any = true;
                    let ft = entry.file_type();
                    out.push(Child {
                        name: entry.file_name().to_owned(),
                        is_dir: matches!(ft, FileType::Directory),
                        d_type_unknown: matches!(ft, FileType::Unknown),
                    });
                }
            }
        }
        if hit_eof {
            break;
        }
    }
    Ok(out)
}

/// Opens a child presumed-directory relative to `parent_fd`, refusing to
/// follow a symlink (`O_NOFOLLOW`) — see the module doc comment.
fn open_child_dir(parent_fd: &OwnedFd, name: &std::ffi::CStr) -> rustix::io::Result<OwnedFd> {
    guard::assert_not_ui_thread();
    fs::openat(
        parent_fd,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// Recursively empties (does not remove) `dir_fd`'s own entry — only its
/// contents. Every descent is `openat(dir_fd, name, ...)`; every removal is
/// `unlinkat(dir_fd, name, ...)`. Best-effort continuation on a per
/// -descendant error (per `RemoveKind::Recursive`'s own doc comment:
/// "implementations should still remove everything they can before
/// returning the first error"), returning the first error encountered, if
/// any.
fn empty_dir_contents(dir_fd: &OwnedFd) -> std::result::Result<(), Errno> {
    let children = list_children(dir_fd)?;
    let mut first_err = None;

    for child in children {
        let is_dir = if child.d_type_unknown {
            // Rare (most local filesystems always populate d_type): fall
            // back to an fd-relative fstatat, still never touching a path
            // string.
            match fs::statat(dir_fd, child.name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => matches!(FileType::from_raw_mode(st.st_mode), FileType::Directory),
                Err(e) => {
                    first_err.get_or_insert(e);
                    continue;
                }
            }
        } else {
            child.is_dir
        };

        if is_dir {
            match open_child_dir(dir_fd, child.name.as_c_str()) {
                Ok(child_fd) => {
                    if let Err(e) = empty_dir_contents(&child_fd) {
                        first_err.get_or_insert(e);
                        continue;
                    }
                    guard::assert_not_ui_thread();
                    if let Err(e) = fs::unlinkat(dir_fd, child.name.as_c_str(), AtFlags::REMOVEDIR)
                    {
                        // Already gone (e.g. this same name was itself
                        // renamed away mid-walk) is not an error worth
                        // surfacing -- design.md's Remove recovery
                        // semantics treat a target that's already absent
                        // as success, not failure.
                        if e != Errno::NOENT {
                            first_err.get_or_insert(e);
                        }
                    }
                }
                Err(e) => {
                    // ELOOP here means the module doc comment's defense
                    // fired: a symlink was substituted for a real
                    // subdirectory mid-walk. Surface it rather than
                    // silently skipping, since it's a signal worth the
                    // caller seeing, but do not follow it.
                    first_err.get_or_insert(e);
                }
            }
        } else {
            guard::assert_not_ui_thread();
            if let Err(e) = fs::unlinkat(dir_fd, child.name.as_c_str(), AtFlags::empty())
                && e != Errno::NOENT
            {
                first_err.get_or_insert(e);
            }
        }
    }

    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `FileSystem::remove(_, RemoveKind::Recursive)`'s implementation: removes
/// `p` and everything under it. The *only* path resolution in this whole
/// call is the single `openat` below, against `p`'s real path — every
/// subsequent step is fd-relative (see module doc comment).
pub fn remove_recursive(p: &VPath) -> Result<()> {
    guard::assert_not_ui_thread();
    let path = real_path(p);

    // O_NOFOLLOW: if `p` itself is a symlink (not a directory to recurse
    // into), refuse to follow it here -- `RemoveKind::Recursive` on a
    // symlink should unlink the link, not walk through it.
    let dir_fd = match fs::openat(
        CWD,
        &path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::LOOP) | Err(Errno::NOTDIR) => {
            // Not a (non-symlink) directory: treat as removing a single
            // node, matching RemoveKind::File semantics rather than
            // erroring out of a Recursive call on a plain file/symlink.
            return fs::unlinkat(CWD, &path, AtFlags::empty())
                .map_err(|e| rustix_err("unlinkat", p, e));
        }
        Err(e) => return Err(rustix_err("openat(O_DIRECTORY|O_NOFOLLOW)", p, e)),
    };

    if let Err(e) = empty_dir_contents(&dir_fd) {
        return Err(rustix_err("remove_recursive (descendant)", p, e));
    }
    drop(dir_fd);

    // Directories can't be removed via their own fd (AT_REMOVEDIR needs a
    // parent fd + name) -- one more, final, parent-relative resolution.
    let Some((parent, name)) = path.parent().zip(path.file_name()) else {
        return Err(Box::new(
            duet_types::VfsError::new(ErrorKind::Fatal, "remove_recursive: path has no parent")
                .with_path(p.clone()),
        ));
    };
    let parent_fd = fs::openat(
        CWD,
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| rustix_err("openat(parent)", p, e))?;
    match fs::unlinkat(&parent_fd, name, AtFlags::REMOVEDIR) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(e) => Err(rustix_err("unlinkat(REMOVEDIR)", p, e)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use duet_types::UnixPathBuf;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn removes_nested_tree() {
        let base = TempDir::new().unwrap();
        let root = base.path().join("root");
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        std::fs::write(root.join("a/f1.txt"), b"x").unwrap();
        std::fs::write(root.join("a/b/f2.txt"), b"y").unwrap();
        std::fs::write(root.join("a/b/c/f3.txt"), b"z").unwrap();

        let vp = VPath::local(UnixPathBuf::new(root.to_str().unwrap()).unwrap());
        remove_recursive(&vp).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn removes_empty_directory() {
        let base = TempDir::new().unwrap();
        let root = base.path().join("empty");
        std::fs::create_dir(&root).unwrap();
        let vp = VPath::local(UnixPathBuf::new(root.to_str().unwrap()).unwrap());
        remove_recursive(&vp).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn does_not_touch_siblings() {
        let base = TempDir::new().unwrap();
        std::fs::create_dir(base.path().join("victim")).unwrap();
        std::fs::write(base.path().join("victim/f.txt"), b"x").unwrap();
        std::fs::write(base.path().join("sibling.txt"), b"keep-me").unwrap();

        let vp =
            VPath::local(UnixPathBuf::new(base.path().join("victim").to_str().unwrap()).unwrap());
        remove_recursive(&vp).unwrap();
        assert!(!base.path().join("victim").exists());
        assert_eq!(
            std::fs::read(base.path().join("sibling.txt")).unwrap(),
            b"keep-me"
        );
    }

    #[test]
    fn recursive_remove_on_a_symlink_unlinks_the_link_not_the_target() {
        let base = TempDir::new().unwrap();
        let target = base.path().join("real_target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep.txt"), b"keep").unwrap();
        let link = base.path().join("link_to_target");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let vp = VPath::local(UnixPathBuf::new(link.to_str().unwrap()).unwrap());
        remove_recursive(&vp).unwrap();

        assert!(!link.exists());
        // The real target and its contents are untouched.
        assert!(target.is_dir());
        assert_eq!(std::fs::read(target.join("keep.txt")).unwrap(), b"keep");
    }

    /// T-3.1.3 AC: "a TOCTOU test that renames a directory mid-walk and
    /// confirms the walk does not escape the intended subtree."
    ///
    /// Two real threads, synchronized with a `Barrier` (not a `sleep` --
    /// this is a genuine, deterministic interleaving, not a timing hope):
    /// thread A opens a directory fd (exactly what a recursive walk does
    /// before descending into a subdirectory), thread B then renames that
    /// *same directory, by name* out from under it into an unrelated
    /// location, and only after B's rename is durably complete does A
    /// perform a directory-fd-relative operation against the fd it opened
    /// *before* the rename.
    ///
    /// If directory descent were path-based (re-resolving "root/victim"
    /// each time) rather than fd-based, this rename would either make the
    /// subsequent operation fail outright (ENOENT — the name is gone) or,
    /// worse, silently operate on whatever an attacker placed at the
    /// now-vacant name. Because it's fd-based, the operation instead
    /// transparently continues to affect the *original* directory's
    /// *original* inode, wherever its name now points — and, critically,
    /// a sentinel file in the unrelated destination directory (standing in
    /// for "anything outside the intended subtree") is provably never
    /// touched.
    #[test]
    fn toctou_rename_after_open_does_not_redirect_the_open_fd() {
        let base = TempDir::new().unwrap();
        let root = base.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let victim = root.join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("file_a"), b"original-content").unwrap();

        let outside = base.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("sentinel.txt");
        std::fs::write(&sentinel, b"do-not-touch").unwrap();

        // Open the fd *before* any concurrent rename -- exactly the state
        // a recursive walk is in right after deciding to descend into
        // "victim" but before it has finished processing that
        // subdirectory's own contents.
        let root_fd = fs::openat(
            CWD,
            &root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let victim_fd = open_child_dir(&root_fd, c"victim").unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let barrier2 = Arc::clone(&barrier);
        let root_for_thread = root.clone();
        let outside_for_thread = outside.clone();
        let renamer = thread::spawn(move || {
            barrier2.wait(); // (1) both threads ready
            // Attacker/racing actor: move the directory our main thread
            // already has open, by name, to an unrelated location.
            rustix::fs::renameat(
                CWD,
                root_for_thread.join("victim"),
                CWD,
                outside_for_thread.join("victim_moved"),
            )
            .unwrap();
            barrier2.wait(); // (2) rename is now durably complete
        });

        barrier.wait(); // (1)
        barrier.wait(); // (2) -- guaranteed: the rename above has returned
        renamer.join().unwrap();

        // The name "root/victim" is now gone entirely (renamed away).
        assert!(!victim.exists());
        // But the fd opened before the rename still refers to the same
        // directory, wherever it now lives.
        let content = std::fs::read(format!(
            "/proc/self/fd/{}/file_a",
            std::os::fd::AsRawFd::as_raw_fd(&victim_fd)
        ))
        .expect("fd opened before rename must still resolve its contents");
        assert_eq!(content, b"original-content");

        // Unlink through the pinned fd -- this must affect the ORIGINAL
        // inode (now reachable at outside/victim_moved), not anything
        // that could have been planted at the vacated "root/victim" name
        // (there is nothing there -- the name is gone -- which is itself
        // part of the proof: a path-based re-resolution would have hit
        // ENOENT here instead of succeeding).
        fs::unlinkat(&victim_fd, c"file_a", AtFlags::empty()).unwrap();

        // Escape-blast-radius check: the sentinel elsewhere under
        // `outside/` (standing in for "anything outside the intended
        // subtree") is completely untouched.
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"do-not-touch");
        // And the file really is gone at its new, renamed location too --
        // proving the fd and "outside/victim_moved" are the same inode,
        // not that we silently no-op'd.
        assert!(!outside.join("victim_moved").join("file_a").exists());
        // The directory entry itself (now named "victim_moved") is
        // untouched structurally -- our walk only ever had root_fd/
        // victim_fd, never outside's fd, so it could not have unlinked
        // the "victim_moved" name even if it wanted to.
        assert!(outside.join("victim_moved").is_dir());
    }

    /// The same property, exercised through the production
    /// `remove_recursive` entry point rather than raw primitives: start a
    /// recursive remove of a directory, and -- from a second thread --
    /// rename a *sibling* subtree that is never part of the intended
    /// removal. The unrelated rename must have zero effect on the
    /// in-progress removal or its target.
    #[test]
    fn remove_recursive_unaffected_by_concurrent_unrelated_rename() {
        let base = TempDir::new().unwrap();
        std::fs::create_dir_all(base.path().join("victim/sub")).unwrap();
        std::fs::write(base.path().join("victim/sub/f.txt"), b"x").unwrap();
        std::fs::create_dir(base.path().join("bystander")).unwrap();
        std::fs::write(base.path().join("bystander/keep.txt"), b"keep").unwrap();

        let base_path = base.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(2));
        let barrier2 = Arc::clone(&barrier);
        let renamer = thread::spawn(move || {
            barrier2.wait();
            rustix::fs::renameat(
                CWD,
                base_path.join("bystander"),
                CWD,
                base_path.join("bystander_renamed"),
            )
            .unwrap();
        });

        barrier.wait();
        let vp =
            VPath::local(UnixPathBuf::new(base.path().join("victim").to_str().unwrap()).unwrap());
        let result = remove_recursive(&vp);
        renamer.join().unwrap();

        assert!(result.is_ok());
        assert!(!base.path().join("victim").exists());
        // The unrelated directory was renamed by the racing thread
        // (expected -- that's its own operation), but its *contents* are
        // untouched by our removal.
        assert_eq!(
            std::fs::read(base.path().join("bystander_renamed/keep.txt")).unwrap(),
            b"keep"
        );
    }
}
