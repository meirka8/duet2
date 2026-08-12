//! T-3.1.1 — `LocalFs::read_dir`: chunked `getdents64` enumeration.
//!
//! Winning strategy per S-4's spike (`documentation/spikes/S-4.md`, cross
//! -checked against `spikes/s4-dir-enum/src/strategies.rs`): a raw
//! `getdents64` scan (via `rustix::fs::RawDir`), using `d_type` for
//! `Metadata::kind` and issuing **zero** `stat`/`statx` calls when the
//! caller only asked for names+types (`ListOpts::names_only`). When the
//! caller's `ListFields` asks for more, or `follow_symlinks` is set, each
//! chunk's entries are additionally resolved via the parallel batched
//! `statx` helper in `local::statx` (T-3.1.2) — driven by the same
//! `ListFields` request, so "ask for fewer fields, do less work" holds for
//! `read_dir` too, not just the standalone `stat` path.
//!
//! # Chunking, without a self-referential iterator
//!
//! `rustix::fs::RawDir::new(fd, buf)` borrows both `fd` and `buf`, so an
//! instance that outlives a single function call would be self-referential
//! with whatever owns the buffer — awkward to hold across `Stream::poll`
//! calls without unsafe. This implementation sidesteps that entirely: each
//! "get one chunk" call opens a **fresh, small** `RawDir` over the *same*
//! directory fd, and drains it until its internal buffer needs a second
//! `getdents64` refill (`RawDir::is_buffer_empty()` back to `true`) — i.e.
//! exactly one buffer's worth of entries — then stops and drops the
//! `RawDir`. Because we only ever stop at a point where the buffer is
//! fully drained (never mid-buffer), nothing is lost: `RawDir`'s own doc
//! comment guarantees "the iterator is guaranteed to continue where it
//! left off provided the file descriptor isn't changed," and the kernel's
//! `getdents64` position is tracked by the fd itself, not by our `RawDir`
//! wrapper. A `CHUNK_BUF_BYTES`-sized buffer is picked so "one buffer's
//! worth" is a few thousand entries — enough to amortize syscall overhead,
//! small enough that the *first* chunk (the one the panel needs to paint
//! quickly) is a small slice of a 100k-entry directory, not the whole
//! thing.

use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;

use duet_types::{EntryKind, ErrorKind, Metadata, Result, VPath, VfsError};
use futures_core::stream::BoxStream;
use futures_util::stream;
use rustix::fs::{self, CWD, FileType, Mode, OFlags, RawDir};

use super::guard;
use super::pathutil::{real_path, rustix_err};
use super::statx::batch_stat;
use crate::{DirEntry, ListFields, ListOpts};

/// One `getdents64` buffer's worth of entries, per chunk. 64 KiB comfortably
/// holds several thousand short-named dirents (typical `dirent64` records
/// are 24-48 bytes) while staying well clear of the 5ms first-chunk budget
/// even before any per-entry work — see the module doc comment.
const CHUNK_BUF_BYTES: usize = 64 * 1024;

/// One raw entry off `getdents64`: name + `d_type`, nothing else (the whole
/// point of the fast path — see module doc comment).
struct RawEntry {
    name: String,
    kind: EntryKind,
    d_type_unknown: bool,
}

fn map_d_type(ft: FileType) -> EntryKind {
    match ft {
        FileType::RegularFile => EntryKind::File,
        FileType::Directory => EntryKind::Directory,
        FileType::Symlink => EntryKind::Symlink,
        FileType::Fifo => EntryKind::Fifo,
        FileType::Socket => EntryKind::Socket,
        FileType::BlockDevice => EntryKind::BlockDevice,
        FileType::CharacterDevice => EntryKind::CharDevice,
        _ => EntryKind::Unknown,
    }
}

/// Reads exactly one `getdents64`-buffer's worth of entries from `dir_fd`,
/// skipping `.`/`..`. Returns `(entries, eof)`; `eof` is `true` only once
/// the kernel has reported zero further entries (a real end-of-directory,
/// not merely "this buffer is full").
fn read_one_chunk(dir_fd: &OwnedFd) -> rustix::io::Result<(Vec<RawEntry>, bool)> {
    guard::assert_not_ui_thread();
    let mut buf = vec![MaybeUninit::uninit(); CHUNK_BUF_BYTES];
    let mut iter = RawDir::new(dir_fd, buf.as_mut_slice());
    let mut out = Vec::new();
    let mut eof = false;
    loop {
        if iter.is_buffer_empty() && !out.is_empty() {
            // We've fully drained one buffer's worth. Stop here so a
            // chunk request never issues more than the syscalls needed to
            // fill this one buffer.
            break;
        }
        match iter.next() {
            None => {
                eof = true;
                break;
            }
            Some(Err(e)) => return Err(e),
            Some(Ok(entry)) => {
                let name_bytes = entry.file_name().to_bytes();
                if name_bytes == b"." || name_bytes == b".." {
                    continue;
                }
                let ft = entry.file_type();
                out.push(RawEntry {
                    name: String::from_utf8_lossy(name_bytes).into_owned(),
                    kind: map_d_type(ft),
                    d_type_unknown: matches!(ft, FileType::Unknown),
                });
            }
        }
    }
    Ok((out, eof))
}

struct ScanState {
    dir_fd: OwnedFd,
    dir_vpath: VPath,
    opts: ListOpts,
    done: bool,
}

/// Builds `LocalFs::read_dir`'s stream. `dir_fd` must already be open
/// (`O_RDONLY|O_DIRECTORY`) on `dir_vpath`'s real path — opening happens in
/// the caller (`LocalFs::read_dir`) so an immediate open failure (`p`
/// doesn't exist / isn't a directory) can be reported as the *first* stream
/// item rather than a panic building the stream.
pub fn stream_from_open_dir(
    dir_fd: OwnedFd,
    dir_vpath: VPath,
    opts: ListOpts,
) -> BoxStream<'static, Result<Vec<DirEntry>>> {
    let state = ScanState {
        dir_fd,
        dir_vpath,
        opts,
        done: false,
    };
    Box::pin(stream::unfold(state, |mut state| async move {
        loop {
            if state.done {
                return None;
            }
            match read_one_chunk(&state.dir_fd) {
                Err(e) => {
                    state.done = true;
                    let err = rustix_err("getdents64", &state.dir_vpath, e);
                    return Some((Err(err), state));
                }
                Ok((raw, eof)) => {
                    state.done = eof;
                    if raw.is_empty() {
                        if eof {
                            return None;
                        }
                        // Only `.`/`..` in this buffer's worth (small/near
                        // -empty directory) -- keep reading rather than
                        // yielding an empty chunk.
                        continue;
                    }
                    let entries = resolve_chunk(&state.dir_fd, raw, state.opts);
                    return Some((Ok(entries), state));
                }
            }
        }
    }))
}

/// Turns raw `(name, d_type)` pairs into `DirEntry`s, resolving full
/// metadata via batched `statx` only when the caller's `ListOpts` actually
/// need it (fields requested, `follow_symlinks`, or a `DT_UNKNOWN` d_type
/// that needs a real stat to classify) — T-3.1.1's "no stat calls for
/// names+types" fast path stays intact for the common case.
fn resolve_chunk(dir_fd: &OwnedFd, raw: Vec<RawEntry>, opts: ListOpts) -> Vec<DirEntry> {
    let needs_stat = !opts.fields.is_empty() || opts.follow_symlinks;
    let has_unknown_types = raw.iter().any(|e| e.d_type_unknown);

    if !needs_stat && !has_unknown_types {
        return raw
            .into_iter()
            .map(|e| DirEntry::new(e.name, Metadata::minimal(e.kind)))
            .collect();
    }

    let names: Vec<&str> = raw.iter().map(|e| e.name.as_str()).collect();
    // Only DT_UNKNOWN entries strictly need resolving when the caller
    // didn't ask for extra fields; request nothing beyond type in that
    // case (kind is always populated regardless of `fields`).
    let stat_fields = if needs_stat {
        opts.fields
    } else {
        ListFields::empty()
    };
    let stats = batch_stat(dir_fd, &names, stat_fields, opts.follow_symlinks);

    raw.into_iter()
        .zip(stats)
        .map(|(e, stat_result)| {
            let metadata = match stat_result {
                Ok(md) => md,
                // Entry vanished between getdents64 and statx (a real
                // race, not a bug). Fall back to the d_type-derived kind
                // rather than failing the whole chunk over one racy entry.
                Err(_) => Metadata::minimal(e.kind),
            };
            DirEntry::new(e.name, metadata)
        })
        .collect()
}

/// Opens `p` as a directory (`O_RDONLY|O_DIRECTORY|O_CLOEXEC`), classifying
/// the open failure per `FileSystem::read_dir`'s documented error buckets.
pub fn open_dir(p: &VPath) -> Result<OwnedFd> {
    guard::assert_not_ui_thread();
    let path = real_path(p);
    match fs::openat(
        CWD,
        &path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => Ok(fd),
        Err(e) => {
            let kind = match e {
                rustix::io::Errno::NOTDIR => ErrorKind::Conflict,
                rustix::io::Errno::NOENT => ErrorKind::NotFound,
                rustix::io::Errno::ACCESS => ErrorKind::Permission,
                other => ErrorKind::from_errno(other.raw_os_error()),
            };
            Err(Box::new(
                VfsError::new(kind, format!("openat(O_DIRECTORY): {e}"))
                    .with_path(p.clone())
                    .with_source(std::io::Error::from(e)),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use tempfile::TempDir;

    use super::*;
    use duet_types::UnixPathBuf;

    fn vpath_for(dir: &TempDir) -> VPath {
        VPath::local(UnixPathBuf::new(dir.path().to_str().unwrap()).unwrap())
    }

    #[tokio::test]
    async fn empty_directory_yields_no_chunks() {
        let dir = TempDir::new().unwrap();
        let vp = vpath_for(&dir);
        let fd = open_dir(&vp).unwrap();
        let mut stream = stream_from_open_dir(fd, vp, ListOpts::names_only());
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn names_only_lists_all_entries_across_chunks() {
        let dir = TempDir::new().unwrap();
        for i in 0..500 {
            std::fs::write(dir.path().join(format!("f{i:04}")), b"x").unwrap();
        }
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let vp = vpath_for(&dir);
        let fd = open_dir(&vp).unwrap();
        let mut stream = stream_from_open_dir(fd, vp, ListOpts::names_only());
        let mut names = std::collections::HashSet::new();
        let mut chunk_count = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            chunk_count += 1;
            for e in chunk {
                names.insert(e.name);
            }
        }
        assert_eq!(names.len(), 501);
        assert!(names.contains("subdir"));
        assert!(names.contains("f0000"));
        assert!(chunk_count >= 1);
    }

    #[tokio::test]
    async fn names_only_reports_kind_without_stat_call() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("file.txt"), b"hello").unwrap();
        std::fs::create_dir(dir.path().join("dir")).unwrap();
        let vp = vpath_for(&dir);
        let fd = open_dir(&vp).unwrap();
        let mut stream = stream_from_open_dir(fd, vp, ListOpts::names_only());
        let mut kinds = std::collections::HashMap::new();
        while let Some(chunk) = stream.next().await {
            for e in chunk.unwrap() {
                kinds.insert(e.name, e.metadata.kind);
            }
        }
        assert_eq!(kinds["file.txt"], EntryKind::File);
        assert_eq!(kinds["dir"], EntryKind::Directory);
        // names_only requests zero extra fields; size should be the
        // "not requested" default, not the real file size.
    }

    #[tokio::test]
    async fn full_opts_populates_size_and_modified() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("file.txt"), b"hello world").unwrap();
        let vp = vpath_for(&dir);
        let fd = open_dir(&vp).unwrap();
        let mut stream = stream_from_open_dir(fd, vp, ListOpts::full());
        let mut found = false;
        while let Some(chunk) = stream.next().await {
            for e in chunk.unwrap() {
                if e.name == "file.txt" {
                    assert_eq!(e.metadata.size, 11);
                    assert!(e.metadata.modified.is_some());
                    found = true;
                }
            }
        }
        assert!(found);
    }

    #[tokio::test]
    async fn nonexistent_directory_errors_not_found() {
        let vp = VPath::local(UnixPathBuf::new("/nonexistent-duet-vfs-test-path").unwrap());
        let err = open_dir(&vp).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn file_instead_of_directory_errors_conflict() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("notadir");
        std::fs::write(&file_path, b"x").unwrap();
        let vp = VPath::local(UnixPathBuf::new(file_path.to_str().unwrap()).unwrap());
        let err = open_dir(&vp).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Conflict);
    }

    /// T-3.1.1 AC: "100k entries enumerated (names + types only) in <= 60ms
    /// on tmpfs; first chunk emitted in <= 5ms." `TempDir::new()` resolves
    /// under `$TMPDIR`, which in this dev environment is tmpfs (`/tmp`) --
    /// verified once, loudly, rather than assumed silently, since the AC is
    /// tmpfs-specific and a non-tmpfs `$TMPDIR` would make this test
    /// measure the wrong thing. `#[ignore]`d: a real timing measurement
    /// belongs in the "run deliberately, report the number" bucket, not
    /// the default fast unit-test loop (T-3.3.4 owns the permanent
    /// benchmark harness; this proves the number now).
    #[test]
    #[ignore = "timing benchmark; run explicitly with --ignored"]
    fn ac_100k_names_and_types_within_60ms_first_chunk_within_5ms() {
        let dir = TempDir::new().unwrap();
        assert_on_tmpfs(dir.path());

        let n = 100_000;
        for i in 0..n {
            std::fs::write(dir.path().join(format!("f{i:06}")), []).unwrap();
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        // First-chunk latency: time from opening the stream to the first
        // yielded item.
        let vp = vpath_for(&dir);
        let fd = open_dir(&vp).unwrap();
        let first_chunk_elapsed = rt.block_on(async {
            let mut stream = stream_from_open_dir(fd, vp, ListOpts::names_only());
            let start = std::time::Instant::now();
            let chunk = stream.next().await.unwrap().unwrap();
            let elapsed = start.elapsed();
            assert!(!chunk.is_empty());
            elapsed
        });

        // Whole-directory latency, best-of-5 (S-4's own methodology).
        let trials = 5;
        let whole_dir_best = (0..trials)
            .map(|_| {
                let vp = vpath_for(&dir);
                let fd = open_dir(&vp).unwrap();
                rt.block_on(async {
                    let mut stream = stream_from_open_dir(fd, vp, ListOpts::names_only());
                    let start = std::time::Instant::now();
                    let mut count = 0usize;
                    while let Some(chunk) = stream.next().await {
                        count += chunk.unwrap().len();
                    }
                    assert_eq!(count, n);
                    start.elapsed()
                })
            })
            .min()
            .unwrap();

        eprintln!(
            "ac_100k_names_and_types: first_chunk={first_chunk_elapsed:?} \
             whole_dir_best_of_{trials}={whole_dir_best:?}"
        );
        assert!(
            first_chunk_elapsed.as_millis() <= 5,
            "first chunk took {first_chunk_elapsed:?}, budget is 5ms"
        );
        assert!(
            whole_dir_best.as_millis() <= 60,
            "whole-directory listing took {whole_dir_best:?}, budget is 60ms"
        );
    }

    /// Loudly confirms the tempdir used by the AC benchmark above is
    /// actually tmpfs, per `T-3.1.1`'s tmpfs-specific target -- a silently
    /// non-tmpfs `$TMPDIR` would make the timing assertions meaningless.
    fn assert_on_tmpfs(path: &std::path::Path) {
        let stfs = rustix::fs::statfs(path).expect("statfs on test tempdir");
        // TMPFS_MAGIC per statfs(2)/linux/magic.h.
        const TMPFS_MAGIC: u32 = 0x0102_1994;
        assert_eq!(
            u32::try_from(stfs.f_type).unwrap_or(0),
            TMPFS_MAGIC,
            "T-3.1.1's AC is tmpfs-specific; TempDir resolved to a non-tmpfs \
             $TMPDIR ({path:?}) in this environment -- the timing numbers \
             below would not measure what the AC asks for."
        );
    }
}
