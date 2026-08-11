//! T-3.1.4 — `AsyncReadSeek`/`AsyncWriteCommit` for `LocalFs`.
//!
//! Reads are a thin poll-based wrapper over `std::fs::File` (see the
//! `local` module doc comment on the blocking-in-async-facade
//! architecture this whole crate uses — the actual thread-offload
//! decision belongs to the caller/shell layer, this module's job is only
//! to make the blocking calls it does make go through the T-3.1.6 guard).
//!
//! Writes implement design.md §9.3's crash-safety journal contract
//! directly (`docs/crash-safety.md`, cross-referenced in the doc comments
//! below): `open_write` never touches the destination path at all — it
//! creates a `.duet-partial-<rand>-<name>` sibling (exclusive create,
//! `O_EXCL`) in the same directory and writes there. Only
//! [`AsyncWriteCommit::commit`] ever touches the destination, via a single
//! `renameat2` that atomically publishes the finished write — the
//! `.duet-partial-*` naming convention and the "never write the
//! destination directly" rule together are exactly what makes "an
//! interrupted write leaves either the old file intact or a clearly
//! -marked partial file" (FR-OPS-07) hold *by construction*: a process
//! killed at any point before `commit` runs has, by definition, not yet
//! called the one function that ever touches `dest_path`.

use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use duet_types::{ErrorKind, Result, VPath, VfsError};
use rustix::fs::{self, AtFlags, CWD, FileType, Mode as RustixMode, OFlags, RenameFlags};
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};

use super::guard;
use super::pathutil::{io_err, partial_sibling_name, real_path, rustix_err, split_parent};
use crate::{AsyncWriteCommit, CommitOutcome, Mode, WriteOpts};

/// The read handle `LocalFs::open_read` returns.
pub struct LocalFile {
    file: std::fs::File,
    pending_seek: Option<io::Result<u64>>,
}

impl AsyncRead for LocalFile {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        guard::assert_not_ui_thread();
        use std::io::Read;
        let this = self.get_mut();
        match this.file.read(buf.initialize_unfilled()) {
            Ok(n) => {
                buf.advance(n);
                Poll::Ready(Ok(()))
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl AsyncSeek for LocalFile {
    fn start_seek(self: Pin<&mut Self>, position: io::SeekFrom) -> io::Result<()> {
        guard::assert_not_ui_thread();
        use std::io::Seek;
        let this = self.get_mut();
        this.pending_seek = Some(this.file.seek(position));
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        match this.pending_seek.take() {
            Some(r) => Poll::Ready(r),
            None => {
                use std::io::Seek;
                Poll::Ready(this.file.stream_position())
            }
        }
    }
}

/// Opens `p` for reading. Rejects directories up front (`Conflict`,
/// matching `FileSystem::open_read`'s documented `EISDIR` behaviour)
/// rather than deferring to the first `read()` call's error, since a
/// directory opens successfully on Linux (`O_RDONLY` with no `O_DIRECTORY`
/// requirement) and only fails once actually read from.
pub fn open_read(p: &VPath) -> Result<LocalFile> {
    guard::assert_not_ui_thread();
    let path = real_path(p);
    let st = fs::statat(CWD, &path, AtFlags::empty()).map_err(|e| rustix_err("statat", p, e))?;
    if matches!(FileType::from_raw_mode(st.st_mode), FileType::Directory) {
        return Err(Box::new(
            VfsError::new(
                ErrorKind::Conflict,
                "open_read: is a directory; use read_dir instead",
            )
            .with_path(p.clone()),
        ));
    }
    let file = std::fs::File::open(&path).map_err(|e| io_err("open", p, e))?;
    Ok(LocalFile {
        file,
        pending_seek: None,
    })
}

/// The write handle `LocalFs::open_write` returns. See module doc comment
/// for the crash-safety argument this type exists to provide.
pub struct LocalWriteHandle {
    file: std::fs::File,
    dest_path: PathBuf,
    temp_path: PathBuf,
    dest_vpath: VPath,
    /// `true` when `WriteOpts::fail_if_exists` was set: `commit` uses
    /// `RENAME_NOREPLACE` so the exclusivity check is atomic against the
    /// final publish, not a separate (racy) check-then-write.
    no_replace: bool,
    bytes_written: u64,
}

impl AsyncWrite for LocalWriteHandle {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        guard::assert_not_ui_thread();
        use std::io::Write;
        let this = self.get_mut();
        match this.file.write(buf) {
            Ok(n) => {
                this.bytes_written += n as u64;
                Poll::Ready(Ok(n))
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        guard::assert_not_ui_thread();
        use std::io::Write;
        Poll::Ready(self.get_mut().file.flush())
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Nothing to release beyond an ordinary flush -- publishing
        // happens in `commit`, not on shutdown (see trait doc comment:
        // "there is no close()-and-forget path").
        self.poll_flush(_cx)
    }
}

#[async_trait]
impl AsyncWriteCommit for LocalWriteHandle {
    async fn commit(self: Box<Self>) -> Result<CommitOutcome> {
        guard::assert_not_ui_thread();
        let this = *self;
        this.file
            .sync_all()
            .map_err(|e| io_err("fsync(temp)", &this.dest_vpath, e))?;
        drop(this.file);

        let mut flags = RenameFlags::empty();
        if this.no_replace {
            flags |= RenameFlags::NOREPLACE;
        }
        if let Err(e) = fs::renameat_with(CWD, &this.temp_path, CWD, &this.dest_path, flags) {
            // Best-effort cleanup: don't leave the temp sibling behind if
            // we know for certain the publish never happened.
            let _ = fs::unlinkat(CWD, &this.temp_path, AtFlags::empty());
            return Err(rustix_err("renameat2(commit)", &this.dest_vpath, e));
        }
        Ok(CommitOutcome {
            atomic: true,
            bytes_written: this.bytes_written,
        })
    }

    async fn abort(self: Box<Self>) -> Result<()> {
        guard::assert_not_ui_thread();
        let this = *self;
        drop(this.file);
        match fs::unlinkat(CWD, &this.temp_path, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
            Err(e) => Err(rustix_err("unlinkat(abort)", &this.dest_vpath, e)),
        }
    }
}

/// Opens `p` for writing per `o` — see [`FileSystem::open_write`]'s
/// documented error buckets and the module doc comment for the crash
/// -safety contract this establishes.
///
/// [`FileSystem::open_write`]: crate::FileSystem::open_write
pub fn open_write(p: &VPath, o: WriteOpts) -> Result<LocalWriteHandle> {
    guard::assert_not_ui_thread();
    let dest_path = real_path(p);
    let Some((parent, file_name)) = split_parent(&dest_path) else {
        return Err(Box::new(
            VfsError::new(ErrorKind::Fatal, "open_write: path has no parent (is root)")
                .with_path(p.clone()),
        ));
    };

    let parent_fd = fs::openat(
        CWD,
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        RustixMode::empty(),
    )
    .map_err(|e| rustix_err("openat(parent)", p, e))?;

    if !o.create {
        fs::statat(&parent_fd, file_name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|e| rustix_err("statat", p, e))?;
    } else if o.fail_if_exists
        && fs::statat(&parent_fd, file_name, AtFlags::SYMLINK_NOFOLLOW).is_ok()
    {
        // Friendly up-front check; the real, race-free enforcement is
        // `RENAME_NOREPLACE` at commit time (see `no_replace` above) --
        // this just avoids doing the write at all when we can already
        // tell it's pointless.
        return Err(Box::new(
            VfsError::new(
                ErrorKind::Conflict,
                "open_write: destination already exists (fail_if_exists)",
            )
            .with_path(p.clone()),
        ));
    }

    let mode = RustixMode::from_raw_mode(o.mode.unwrap_or(Mode::DEFAULT_FILE).bits());
    let temp_name = partial_sibling_name(file_name);
    let temp_fd = fs::openat(
        &parent_fd,
        temp_name.as_os_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        mode,
    )
    .map_err(|e| rustix_err("openat(temp create)", p, e))?;

    let file = std::fs::File::from(temp_fd);
    let temp_path = parent.join(<std::ffi::OsString as AsRef<OsStr>>::as_ref(&temp_name));

    Ok(LocalWriteHandle {
        file,
        dest_path,
        temp_path,
        dest_vpath: p.clone(),
        no_replace: o.fail_if_exists,
        bytes_written: 0,
    })
}

#[cfg(test)]
mod tests {
    use duet_types::UnixPathBuf;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

    use super::*;

    fn vp(dir: &TempDir, name: &str) -> VPath {
        VPath::local(UnixPathBuf::new(&format!("{}/{}", dir.path().display(), name)).unwrap())
    }

    /// Like `Result::unwrap_err`, but doesn't require `T: Debug` -- neither
    /// `LocalFile` nor `LocalWriteHandle` implement it (matching
    /// `crate::null`'s tests, which needed the same helper for the same
    /// reason: `Box<dyn AsyncReadSeek>`/`Box<dyn AsyncWriteCommit>` in the
    /// real trait aren't `Debug` either).
    fn expect_err<T>(r: Result<T>) -> Box<VfsError> {
        match r {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(e) => e,
        }
    }

    #[tokio::test]
    async fn write_then_commit_publishes_atomically() {
        let dir = TempDir::new().unwrap();
        let target = vp(&dir, "out.txt");
        let mut handle = open_write(&target, WriteOpts::overwrite()).unwrap();
        handle.write_all(b"hello world").await.unwrap();
        let outcome = Box::new(handle).commit().await.unwrap();
        assert!(outcome.atomic);
        assert_eq!(outcome.bytes_written, 11);
        assert_eq!(
            std::fs::read(dir.path().join("out.txt")).unwrap(),
            b"hello world"
        );
        // No stray partial file left behind after a successful commit.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with(".duet-partial-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "leftover partial files: {leftovers:?}"
        );
    }

    /// T-3.1.4 AC: "a killed write ... leaves the original file intact and
    /// a `.duet-partial-*` file behind." Simulates a `SIGKILL` (which
    /// never runs `Drop`) via `mem::forget` on a handle that has written
    /// partial data but never called `commit`/`abort` -- exactly the state
    /// a real kill -9 mid-write leaves behind, since a killed process runs
    /// no destructors either.
    #[tokio::test]
    async fn killed_write_leaves_original_intact_and_partial_marked() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("out.txt"), b"ORIGINAL CONTENT").unwrap();
        let target = vp(&dir, "out.txt");

        let mut handle = open_write(&target, WriteOpts::overwrite()).unwrap();
        handle
            .write_all(b"only some of the new bytes")
            .await
            .unwrap();
        // Simulate SIGKILL: no commit, no abort, no Drop side effects.
        std::mem::forget(handle);

        // The original destination is completely untouched.
        assert_eq!(
            std::fs::read(dir.path().join("out.txt")).unwrap(),
            b"ORIGINAL CONTENT"
        );
        // A .duet-partial-* sibling exists, holding the partial write.
        let partials: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with(".duet-partial-"))
            .collect();
        assert_eq!(
            partials.len(),
            1,
            "expected exactly one partial file: {partials:?}"
        );
        let partial_content = std::fs::read(dir.path().join(&partials[0])).unwrap();
        assert_eq!(partial_content, b"only some of the new bytes");
    }

    #[tokio::test]
    async fn abort_removes_temp_file_leaves_original_intact() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("out.txt"), b"ORIGINAL").unwrap();
        let target = vp(&dir, "out.txt");
        let mut handle = open_write(&target, WriteOpts::overwrite()).unwrap();
        handle.write_all(b"discarded").await.unwrap();
        Box::new(handle).abort().await.unwrap();

        assert_eq!(
            std::fs::read(dir.path().join("out.txt")).unwrap(),
            b"ORIGINAL"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with(".duet-partial-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[tokio::test]
    async fn create_new_fails_if_exists_via_atomic_rename_noreplace() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("out.txt"), b"already here").unwrap();
        let target = vp(&dir, "out.txt");
        let err = expect_err(open_write(&target, WriteOpts::create_new()));
        assert_eq!(err.kind(), ErrorKind::Conflict);
    }

    #[tokio::test]
    async fn create_false_on_missing_destination_is_not_found() {
        let dir = TempDir::new().unwrap();
        let target = vp(&dir, "missing.txt");
        let err = expect_err(open_write(
            &target,
            WriteOpts {
                create: false,
                ..WriteOpts::overwrite()
            },
        ));
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn open_read_reads_full_contents_and_seeks() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("in.txt"), b"0123456789").unwrap();
        let mut file = open_read(&vp(&dir, "in.txt")).unwrap();
        let mut buf = String::new();
        file.read_to_string(&mut buf).await.unwrap();
        assert_eq!(buf, "0123456789");

        file.seek(io::SeekFrom::Start(3)).await.unwrap();
        let mut rest = String::new();
        file.read_to_string(&mut rest).await.unwrap();
        assert_eq!(rest, "3456789");
    }

    #[tokio::test]
    async fn open_read_on_directory_is_conflict() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        let err = expect_err(open_read(&vp(&dir, "d")));
        assert_eq!(err.kind(), ErrorKind::Conflict);
    }

    #[tokio::test]
    async fn open_read_missing_file_is_not_found() {
        let dir = TempDir::new().unwrap();
        let err = expect_err(open_read(&vp(&dir, "missing.txt")));
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }
}
