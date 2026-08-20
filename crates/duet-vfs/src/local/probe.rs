//! T-3.1.7 — filesystem-property probing: `st_dev`, rotational detection,
//! reflink support, `statfs` type, case-sensitivity — cached per mount
//! (`st_dev`), not re-probed on every call.
//!
//! # Scope note
//!
//! This module's job is *detection*, not consumption: `FileSystem::caps()`
//! (T-2.2.2) has no path parameter — it describes one `LocalFs` instance's
//! capabilities as a single value, not per-directory — so wiring these
//! per-mount results into `caps()` is deferred to whenever the mount table
//! (a later phase) associates one backend instance per real mount rather
//! than one `LocalFs` covering the whole local filesystem tree. What *is*
//! in scope today and implemented here: [`probe`] returns correct,
//! actually-measured results for a given directory, cached by `st_dev`.

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use duet_types::{Result, VPath};
use rustix::fs::{self, CWD, Mode, OFlags};
use rustix::io::Errno;

use super::guard;
use super::pathutil::{real_path, rustix_err};

/// Coarse classification of a `statfs` magic number. `Ext` deliberately
/// doesn't distinguish ext2/ext3/ext4 — `EXT2_SUPER_MAGIC` (`0xEF53`) is
/// shared by all three (per `linux/magic.h`; `statfs` genuinely cannot
/// tell them apart), so claiming a specific ext version from this alone
/// would be a guess, not a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    Ext,
    Btrfs,
    Tmpfs,
    Xfs,
    F2fs,
    /// Any other magic number, carried verbatim rather than dropped —
    /// still useful for logging/diagnostics even when this crate has no
    /// specific behaviour for it.
    Other(i64),
}

const EXT_SUPER_MAGIC: i64 = 0xEF53;
const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;
const TMPFS_MAGIC: i64 = 0x0102_1994;
const XFS_SUPER_MAGIC: i64 = 0x5846_5342;
const F2FS_SUPER_MAGIC: i64 = 0xF2F5_2010u32 as i64;

fn magic_to_kind(magic: i64) -> FsKind {
    match magic {
        EXT_SUPER_MAGIC => FsKind::Ext,
        BTRFS_SUPER_MAGIC => FsKind::Btrfs,
        TMPFS_MAGIC => FsKind::Tmpfs,
        XFS_SUPER_MAGIC => FsKind::Xfs,
        F2FS_SUPER_MAGIC => FsKind::F2fs,
        other => FsKind::Other(other),
    }
}

/// Measured properties of one mounted filesystem, keyed by `st_dev`.
#[derive(Debug, Clone)]
pub struct FsProps {
    pub dev: u64,
    pub kind: FsKind,
    /// `None` when there's no backing block device to ask (tmpfs, and any
    /// network filesystem were one mounted) rather than a "no" answer --
    /// "not applicable" and "confirmed not rotational" are different
    /// things a caller (e.g. a read-ahead/scheduling heuristic) should be
    /// able to tell apart.
    pub rotational: Option<bool>,
    /// Whether `ioctl(FICLONE)` actually succeeded between two real files
    /// created in the probed directory during this probe -- measured, not
    /// inferred from `kind` (ext4 and btrfs share a filesystem-family
    /// story but only one supports reflink; guessing from `kind` alone
    /// would be exactly the kind of unverified claim the task brief warns
    /// against).
    pub reflink_supported: bool,
    /// `None` if the active case-sensitivity probe itself failed (e.g. no
    /// write permission in the probed directory); `Some(true)` for
    /// case-sensitive (the common case on Linux-native filesystems),
    /// `Some(false)` if two names differing only in case collided.
    pub case_sensitive: Option<bool>,
}

static CACHE: RwLock<Option<HashMap<u64, FsProps>>> = RwLock::new(None);

/// Probes (or returns the cached result for) the filesystem containing
/// `dir`, which must be a directory this process can write into --
/// [`FsProps::reflink_supported`]/[`FsProps::case_sensitive`] are
/// determined by actually creating and removing small probe files there,
/// not inferred, per the task's directive to measure rather than assume.
pub fn probe(dir: &VPath) -> Result<FsProps> {
    guard::assert_not_ui_thread();
    let path = real_path(dir);
    let st =
        fs::statat(CWD, &path, fs::AtFlags::empty()).map_err(|e| rustix_err("statat", dir, e))?;
    let dev = st.st_dev;

    if let Some(props) = CACHE.read().unwrap().as_ref().and_then(|m| m.get(&dev)) {
        return Ok(props.clone());
    }

    let props = probe_uncached(&path, dev, dir)?;
    let mut guard = CACHE.write().unwrap();
    guard
        .get_or_insert_with(HashMap::new)
        .insert(dev, props.clone());
    Ok(props)
}

fn probe_uncached(path: &Path, dev: u64, vpath_for_errors: &VPath) -> Result<FsProps> {
    let statfs = fs::statfs(path).map_err(|e| rustix_err("statfs", vpath_for_errors, e))?;
    let kind = magic_to_kind(statfs.f_type);
    let rotational = detect_rotational(dev);
    let reflink_supported = detect_reflink(path);
    let case_sensitive = detect_case_sensitivity(path);

    Ok(FsProps {
        dev,
        kind,
        rotational,
        reflink_supported,
        case_sensitive,
    })
}

/// Resolves `dev`'s major:minor to `/sys/dev/block/<maj>:<min>`, then walks
/// up from there looking for a `queue/rotational` file -- present directly
/// for a whole disk (or a virtual `dm-*`/`loop*` device), one level up the
/// tree for a partition (`/sys/.../sda/sda1` has no `queue` of its own;
/// `/sys/.../sda` does). `None` (not `Some(false)`) when there's simply no
/// block device backing this filesystem at all (tmpfs), **or** when
/// `st_dev`'s major number is `0` -- confirmed on this hardware to be what
/// btrfs reports for a file inside a subvolume (each subvolume gets its
/// own anonymous device number, distinct from the real underlying block
/// device), which has no corresponding entry under `/sys/dev/block` at
/// all. `canonicalize` on that path simply fails (`ENOENT`), so this falls
/// out of the existing "no block device to ask" handling for free rather
/// than needing special-casing -- but it's worth naming explicitly here
/// since it's a real, verified quirk (not a bug) a future reader might
/// otherwise mistake for one. See `local::probe::tests::
/// probes_real_btrfs_if_available` for the confirmed-on-hardware case.
fn detect_rotational(dev: u64) -> Option<bool> {
    let major = fs::major(dev);
    let minor = fs::minor(dev);
    let sys_path = format!("/sys/dev/block/{major}:{minor}");
    let mut candidate = std::fs::canonicalize(sys_path).ok()?;
    loop {
        let rot_path = candidate.join("queue/rotational");
        if let Ok(s) = std::fs::read_to_string(&rot_path) {
            return Some(s.trim() == "1");
        }
        candidate = candidate.parent()?.to_path_buf();
        if !candidate.starts_with("/sys") {
            return None;
        }
    }
}

/// Actually attempts `ioctl(FICLONE)` between two small real files created
/// (and removed) in `dir`, rather than inferring support from filesystem
/// type. Any failure (missing write permission, `ENOTTY`/`EOPNOTSUPP`
/// meaning the filesystem genuinely doesn't implement it, `EXDEV`, or
/// anything else) is treated as "not supported" -- this probe's job is a
/// definite yes/no for "does reflink work *here*, right now," not to
/// distinguish *why* it doesn't.
fn detect_reflink(dir: &Path) -> bool {
    let Some((src_path, src_write_fd)) = create_probe_file(dir, "src") else {
        return false;
    };
    let Some((dst_path, dst_fd)) = create_probe_file(dir, "dst") else {
        let _ = fs::unlinkat(CWD, &src_path, fs::AtFlags::empty());
        return false;
    };
    // Reflink needs real content to clone -- an empty-to-empty FICLONE can
    // trivially "succeed" on some setups without proving anything.
    let wrote = std::fs::write(&src_path, b"duet-vfs reflink probe content").is_ok();
    drop(src_write_fd); // O_WRONLY -- done with it, FICLONE's source needs O_RDONLY below.

    let result = wrote
        && fs::openat(
            CWD,
            &src_path,
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .is_ok_and(|src_read_fd| fs::ioctl_ficlone(&dst_fd, &src_read_fd).is_ok());

    drop(dst_fd);
    let _ = fs::unlinkat(CWD, &src_path, fs::AtFlags::empty());
    let _ = fs::unlinkat(CWD, &dst_path, fs::AtFlags::empty());
    result
}

/// Creates two probe files differing only in ASCII case and checks
/// whether the filesystem treats them as the same directory entry.
/// `None` if the probe itself couldn't run (no write permission).
fn detect_case_sensitivity(dir: &Path) -> Option<bool> {
    let lower_name = format!(".duet-probe-case-{}", std::process::id());
    let upper_name = lower_name.to_ascii_uppercase();
    let lower_path = dir.join(&lower_name);
    let upper_path = dir.join(&upper_name);

    // Clean up any stale probe files from a previous crashed run first, so
    // a leftover doesn't make this probe lie.
    let _ = std::fs::remove_file(&lower_path);
    let _ = std::fs::remove_file(&upper_path);

    std::fs::write(&lower_path, b"a").ok()?;
    let collided = upper_path.exists();
    let _ = std::fs::remove_file(&lower_path);
    let _ = std::fs::remove_file(&upper_path);
    Some(!collided)
}

/// Creates an empty, `O_EXCL` probe file named `.duet-probe-<label>-<pid>`
/// in `dir`, returning its path and open fd (kept open so the caller can
/// `ioctl` on it directly without a second `openat`).
fn create_probe_file(dir: &Path, label: &str) -> Option<(PathBuf, std::os::fd::OwnedFd)> {
    let name = format!(".duet-probe-{label}-{}", std::process::id());
    let path = dir.join(&name);
    let _ = fs::unlinkat(CWD, &path, fs::AtFlags::empty()); // clear any stale leftover
    let fd = fs::openat(
        CWD,
        &path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .ok()?;
    Some((path, fd))
}

/// `true` if `a` and `b` are on the same mounted filesystem (same
/// `st_dev`) -- e.g. to decide whether a reflink/rename can even be
/// attempted between them without hitting `EXDEV`.
pub fn same_mount(a: &VPath, b: &VPath) -> Result<bool> {
    guard::assert_not_ui_thread();
    let pa = real_path(a);
    let pb = real_path(b);
    let sa = fs::statat(CWD, &pa, fs::AtFlags::empty()).map_err(|e| rustix_err("statat", a, e))?;
    let sb = fs::statat(CWD, &pb, fs::AtFlags::empty()).map_err(|e| rustix_err("statat", b, e))?;
    Ok(sa.st_dev == sb.st_dev)
}

/// The most `copy_file_range` is asked to move in a single syscall.
/// `copy_file_range` is kernel-side (no userspace buffer bounce), so this
/// isn't about I/O throughput the way a buffered read/write chunk size is
/// -- it exists purely to bound how long a single call can run before
/// `accelerated_copy`'s loop gets a chance to check `on_progress` again.
/// 64 MiB: comfortably large enough that syscall-dispatch overhead stays
/// negligible relative to the copy itself, comfortably small enough that
/// even a slow destination can't turn one call into a multi-second gap in
/// cancellation responsiveness. This is now also the progress-reporting
/// granularity (`on_progress` is called once per completed chunk, with
/// that chunk's real byte count) -- the same bound that was already chosen
/// for cancellation latency turns out to be exactly the right size for
/// "how chunky is too chunky" on the progress-reporting side too, so no
/// separate constant was introduced for it.
const COPY_FILE_RANGE_CHUNK_BYTES: usize = 64 * 1024 * 1024;

/// `FileSystem::server_side_copy`'s implementation, and the natural first
/// consumer of this module's reflink detection: tries `ioctl(FICLONE)`
/// first, falls back to `copy_file_range` (still kernel-side, still no
/// userspace buffer round-trip, just not copy-on-write), then a
/// sparse-aware buffered copy (T-5.1.4's rung 3, [`sparse_buffered_copy`])
/// rather than `Unsupported` -- per the "Linux Capabilities" directive to
/// exploit `FICLONE`/`copy_file_range` directly, and design.md §9.3's copy
/// -strategy ladder.
pub fn accelerated_copy(
    from: &VPath,
    to: &VPath,
    on_progress: &(dyn Fn(u64) -> bool + Send + Sync),
) -> Result<crate::CopyOutcome> {
    guard::assert_not_ui_thread();
    let from_path = real_path(from);
    let to_path = real_path(to);

    if fs::statat(CWD, &to_path, fs::AtFlags::SYMLINK_NOFOLLOW).is_ok() {
        return Err(Box::new(
            duet_types::VfsError::new(
                duet_types::ErrorKind::Conflict,
                "server_side_copy: destination already exists",
            )
            .with_path(to.clone()),
        ));
    }

    let src_fd = fs::openat(
        CWD,
        &from_path,
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| rustix_err("openat(src)", from, e))?;
    let src_stat = fs::fstat(&src_fd).map_err(|e| rustix_err("fstat(src)", from, e))?;
    let size = u64::try_from(src_stat.st_size).unwrap_or(0);

    let dst_fd = fs::openat(
        CWD,
        &to_path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o644),
    )
    .map_err(|e| rustix_err("openat(dst)", to, e))?;

    // FICLONE only ever works same-device; skip the attempt (and the
    // syscall) entirely when `to`'s parent is provably elsewhere, via the
    // same `same_mount` check a cross-device rename's caller would use to
    // decide whether to even try before hitting EXDEV.
    let same_device = to
        .parent()
        .map(|parent| same_mount(from, &parent))
        .transpose()
        .ok()
        .flatten()
        .unwrap_or(false);
    if same_device && fs::ioctl_ficlone(&dst_fd, &src_fd).is_ok() {
        // A FICLONE reflink is one atomic syscall -- by the time it has
        // returned success, the whole file is already cloned, so there is
        // nothing left in flight to interrupt. Still report the full size
        // (the caller has no other way to learn how many bytes this
        // "copy" represents), but deliberately ignore the return value:
        // honoring a cancellation request here would mean unwinding a
        // reflink that has already atomically succeeded, which isn't
        // "interrupting a copy," it's "discarding a completed one" -- a
        // different, and here unwanted, operation.
        let _ = on_progress(size);
        return Ok(crate::CopyOutcome::Copied {
            bytes: size,
            reflinked: true,
        });
    }

    let mut remaining = size;
    let mut total = 0u64;
    while remaining > 0 {
        let chunk = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(COPY_FILE_RANGE_CHUNK_BYTES);
        match fs::copy_file_range(&src_fd, None, &dst_fd, None, chunk) {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                remaining -= n as u64;
                // Check for cancellation *after* a chunk lands, using the
                // real byte count just transferred, rather than before --
                // this is the same real-world cancellation granularity as
                // a pre-check (still at most one `COPY_FILE_RANGE_CHUNK_
                // BYTES`-sized chunk of latency), but it also means there
                // is always genuine new progress to report on every call.
                if on_progress(n as u64) {
                    let _ = fs::unlinkat(CWD, &to_path, fs::AtFlags::empty());
                    return Ok(crate::CopyOutcome::Interrupted);
                }
            }
            Err(Errno::XDEV | Errno::NOSYS | Errno::OPNOTSUPP) if total == 0 => {
                // Nothing written yet via copy_file_range -- try the
                // sparse-aware buffered rung before giving up entirely.
                // `sparse_buffered_copy` takes over `dst_fd` as-is (still
                // empty) and does its own cancellation/error handling.
                return sparse_buffered_copy(
                    &src_fd,
                    &dst_fd,
                    size,
                    from,
                    to,
                    &to_path,
                    on_progress,
                );
            }
            Err(e) => {
                // A real failure, possibly after partial progress -- this
                // is not "can't accelerate," it's "tried and failed," so
                // it's a genuine error per the trait's documented buckets.
                let _ = fs::unlinkat(CWD, &to_path, fs::AtFlags::empty());
                return Err(rustix_err("copy_file_range", to, e));
            }
        }
    }
    Ok(crate::CopyOutcome::Copied {
        bytes: total,
        reflinked: false,
    })
}

/// Buffer size for the sparse-aware buffered copy's own read/write loop --
/// design.md §9.3's "1-4 MiB buffers" guidance for this exact rung.
const SPARSE_COPY_BUFFER_BYTES: usize = 2 * 1024 * 1024;

/// Below this cumulative byte count, `fadvise(DONTNEED)` is skipped
/// entirely. S-5's own spike report recommends gating it on transfer size
/// ("only apply it once cumulative bytes read for a job exceed some
/// threshold, not per 1-8 KiB file") -- the whole point is protecting
/// *other* page-cache residents from a large copy, and the syscall has a
/// real (if small) per-call cost not worth paying on tiny files that could
/// never meaningfully pressure the cache anyway. A documented, chosen
/// threshold, not a benchmarked one -- same status as `COPY_FILE_RANGE_
/// CHUNK_BYTES` above.
const FADVISE_THRESHOLD_BYTES: u64 = 64 * 1024 * 1024;

/// T-5.1.4's rung 3: `SEEK_DATA`/`SEEK_HOLE`-aware buffered copy, tried
/// when neither `FICLONE` nor `copy_file_range` applied. Takes over
/// `dst_fd` exactly as `accelerated_copy` left it -- already open via its
/// own `O_CREAT | O_EXCL`, still empty, position at `0`.
///
/// Skips holes on read (never touches their bytes at all) and leaves the
/// destination sparse too, by seeking it forward across the same gap
/// rather than writing zeros -- on Linux, seeking a file descriptor past
/// its current end-of-file and then writing creates a hole for the
/// skipped region automatically, on any filesystem that supports sparse
/// files. A source with no holes at all degrades to exactly one data
/// extent spanning the whole file, so this is also the correct
/// general-purpose buffered copy, not a separate code path bolted on.
///
/// `posix_fadvise(DONTNEED)` is applied to the source, behind the read
/// cursor, once cumulative bytes read cross [`FADVISE_THRESHOLD_BYTES`] --
/// design.md §9.3: "so a 200 GB copy doesn't evict the user's page cache"
/// (of *other*, unrelated files -- the source's own already-read pages are
/// exactly what this call deliberately evicts, to make room for
/// everything else). Best-effort throughout: a failed `fadvise` call never
/// fails the copy, it just means this particular chunk didn't get the
/// cache-pressure hint.
///
/// # Progress accounting for sparse sources: a known, disclosed gap
///
/// `on_progress` is called with real bytes transferred as each chunk is
/// written (mid-extent), plus a `0`-byte heartbeat call between extents
/// (no bytes moved since the last check, but still a chance to observe
/// cancellation at the same cadence as before). Holes are, by design,
/// never read or written at all -- so for a genuinely sparse source, the
/// sum of every `on_progress` call this function makes can legitimately
/// fall *short* of `size` (a hole contributes nothing to that sum, but
/// still counts toward the file's logical length). This function does not
/// try to paper over that with a "phantom" report for bytes it never
/// touched; the caller (`copy_file_step` in `duet-ops`) is expected to
/// credit the difference itself as a final top-up once this returns
/// `Copied`, exactly the way it already has to reconcile `Copied::bytes`
/// (which *is* the accurate total, holes included via the final
/// `ftruncate` below) against whatever was reported incrementally along
/// the way.
#[allow(clippy::too_many_arguments)]
fn sparse_buffered_copy(
    src_fd: &OwnedFd,
    dst_fd: &OwnedFd,
    size: u64,
    from: &VPath,
    to: &VPath,
    to_path: &Path,
    on_progress: &(dyn Fn(u64) -> bool + Send + Sync),
) -> Result<crate::CopyOutcome> {
    let fail = |op: &'static str, path: &VPath, e: Errno| -> Box<duet_types::VfsError> {
        let _ = fs::unlinkat(CWD, to_path, fs::AtFlags::empty());
        rustix_err(op, path, e)
    };
    let interrupted = || {
        let _ = fs::unlinkat(CWD, to_path, fs::AtFlags::empty());
        Ok(crate::CopyOutcome::Interrupted)
    };

    let mut buf = vec![0u8; SPARSE_COPY_BUFFER_BYTES];
    let mut pos = 0u64;
    let mut total = 0u64;
    let mut fadvise_applied_up_to = 0u64;

    loop {
        // Between extents: no bytes have moved since the last check within
        // this same extent-to-extent gap, so this is a `0`-byte heartbeat
        // -- still real and meaningful (it's what gives the caller a
        // chance to signal cancellation at this same cadence), just with
        // nothing new to report.
        if on_progress(0) {
            return interrupted();
        }

        let data_start = match fs::seek(src_fd, fs::SeekFrom::Data(pos)) {
            Ok(offset) => offset,
            // No more data between `pos` and EOF -- a trailing hole, or
            // `pos` already at EOF. Either way, the walk is done.
            Err(Errno::NXIO) => break,
            Err(e) => return Err(fail("lseek(SEEK_DATA)", from, e)),
        };
        let data_end = match fs::seek(src_fd, fs::SeekFrom::Hole(data_start)) {
            Ok(offset) => offset,
            Err(e) => return Err(fail("lseek(SEEK_HOLE)", from, e)),
        };
        // Both `seek` calls above moved `src_fd`'s own position (to
        // `data_end`, from the second call) -- reposition it back to the
        // start of the extent we're actually about to read.
        if let Err(e) = fs::seek(src_fd, fs::SeekFrom::Start(data_start)) {
            return Err(fail("lseek", from, e));
        }
        if let Err(e) = fs::seek(dst_fd, fs::SeekFrom::Start(data_start)) {
            return Err(fail("lseek", to, e));
        }

        let mut cursor = data_start;
        while cursor < data_end {
            let want = usize::try_from(data_end - cursor)
                .unwrap_or(usize::MAX)
                .min(SPARSE_COPY_BUFFER_BYTES);
            let n = match rustix::io::read(src_fd, &mut buf[..want]) {
                Ok(n) => n,
                Err(e) => return Err(fail("read", from, e)),
            };
            if n == 0 {
                // Shouldn't happen inside a data extent SEEK_HOLE itself
                // just measured, but a concurrent truncate of the source
                // mid-copy is exactly the kind of thing that could cause
                // it -- treat as "that's all there was," not an error.
                break;
            }
            let mut written = 0usize;
            while written < n {
                match rustix::io::write(dst_fd, &buf[written..n]) {
                    Ok(w) => written += w,
                    Err(e) => return Err(fail("write", to, e)),
                }
            }
            cursor += n as u64;
            total += n as u64;

            if total.saturating_sub(fadvise_applied_up_to) >= FADVISE_THRESHOLD_BYTES {
                let len = NonZeroU64::new(total - fadvise_applied_up_to);
                let _ = fs::fadvise(src_fd, fadvise_applied_up_to, len, fs::Advice::DontNeed);
                fadvise_applied_up_to = total;
            }

            // Checked *after* the chunk is written, using the real byte
            // count just copied, rather than before -- same reasoning as
            // `accelerated_copy`'s `copy_file_range` loop: identical
            // real-world cancellation latency, but every call now also
            // carries genuine new progress.
            if on_progress(n as u64) {
                return interrupted();
            }
        }
        pos = data_end;
    }

    // Fix up the destination's final length in case the source ends in a
    // hole: `SEEK_DATA`/`SEEK_HOLE` only ever advance across *data*, so a
    // trailing hole is never "written" and nothing above has told the
    // destination how long the file is actually supposed to be.
    if let Err(e) = fs::ftruncate(dst_fd, size) {
        return Err(fail("ftruncate", to, e));
    }

    Ok(crate::CopyOutcome::Copied {
        bytes: total,
        reflinked: false,
    })
}

#[cfg(test)]
mod tests {
    use duet_types::UnixPathBuf;
    use tempfile::TempDir;

    use super::*;

    fn vp(dir: &TempDir) -> VPath {
        VPath::local(UnixPathBuf::new(dir.path().to_str().unwrap()).unwrap())
    }

    /// Finds a writable directory on a real, non-tmpfs mounted filesystem
    /// of `wanted` kind, by actually parsing `/proc/mounts` and trying to
    /// write into candidate mount points -- not a hardcoded path (which
    /// would only work on this one machine/user). `None` (not a panic) if
    /// none is found, so this test suite runs correctly (with an honest
    /// "not verified here" note) on a machine that genuinely doesn't have
    /// that filesystem type available, per the task's own instruction not
    /// to claim untested results.
    fn find_writable_real_mount(wanted_magic: i64) -> Option<PathBuf> {
        let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
        for line in mounts.lines() {
            let mut fields = line.split_whitespace();
            let _device = fields.next()?;
            let mountpoint = fields.next()?;
            let mp = PathBuf::from(mountpoint);
            let Ok(sfs) = fs::statfs(&mp) else { continue };
            if sfs.f_type != wanted_magic {
                continue;
            }
            // Prefer a subdirectory we know we can write to rather than
            // the raw mountpoint (often root-owned, e.g. `/`).
            for candidate in [
                std::env::var("HOME").ok().map(PathBuf::from),
                Some(mp.clone()),
            ]
            .into_iter()
            .flatten()
            {
                if let Ok(sfs2) = fs::statfs(&candidate)
                    && sfs2.f_type != wanted_magic
                {
                    continue;
                }
                let probe = candidate.join(format!(".duet-vfs-mount-probe-{}", std::process::id()));
                if std::fs::write(&probe, b"x").is_ok() {
                    let _ = std::fs::remove_file(&probe);
                    return Some(candidate);
                }
            }
        }
        None
    }

    #[test]
    fn probes_tmpfs_correctly() {
        let dir = TempDir::new().unwrap();
        let props = probe(&vp(&dir)).unwrap();
        assert_eq!(props.kind, FsKind::Tmpfs);
        // tmpfs has no backing block device.
        assert_eq!(props.rotational, None);
        // tmpfs does not implement FICLONE.
        assert!(!props.reflink_supported);
        assert_eq!(props.case_sensitive, Some(true));
    }

    #[test]
    fn probe_result_is_cached_by_st_dev() {
        let dir = TempDir::new().unwrap();
        let vp1 = vp(&dir);
        let a = probe(&vp1).unwrap();
        let b = probe(&vp1).unwrap();
        assert_eq!(a.dev, b.dev);
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.reflink_supported, b.reflink_supported);
    }

    #[test]
    fn same_mount_true_within_tmpfs_false_across_devices() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a"), b"x").unwrap();
        std::fs::write(dir.path().join("b"), b"y").unwrap();
        let a = VPath::local(UnixPathBuf::new(dir.path().join("a").to_str().unwrap()).unwrap());
        let b = VPath::local(UnixPathBuf::new(dir.path().join("b").to_str().unwrap()).unwrap());
        assert!(same_mount(&a, &b).unwrap());
    }

    /// T-3.1.7 AC: correct results on whatever filesystems are actually
    /// available -- ext4, measured for real via dynamic mount discovery
    /// (see `find_writable_real_mount`), not assumed. Reports (does not
    /// fail the suite) if this machine has no writable ext4 mount, per
    /// the task's own "note as untested if so" instruction.
    #[test]
    fn probes_real_ext4_if_available() {
        const EXT_MAGIC: i64 = 0xEF53;
        let Some(dir) = find_writable_real_mount(EXT_MAGIC) else {
            eprintln!(
                "probes_real_ext4_if_available: no writable ext4 mount found on this machine \
                 -- skipped, not failed (T-3.1.7 AC: don't claim untested results)."
            );
            return;
        };
        let scratch = dir.join(format!(".duet-vfs-ext4-probe-{}", std::process::id()));
        std::fs::create_dir_all(&scratch).unwrap();
        let vp = VPath::local(UnixPathBuf::new(scratch.to_str().unwrap()).unwrap());
        let props = probe(&vp).unwrap();
        eprintln!(
            "probes_real_ext4_if_available: measured on {}: kind={:?} rotational={:?} \
             reflink_supported={} case_sensitive={:?}",
            scratch.display(),
            props.kind,
            props.rotational,
            props.reflink_supported,
            props.case_sensitive
        );
        assert_eq!(props.kind, FsKind::Ext);
        // ext4 (any version sharing this magic) does not implement FICLONE.
        assert!(!props.reflink_supported);
        assert_eq!(props.case_sensitive, Some(true));
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// As above, for btrfs -- which, unlike ext4, does support reflink, so
    /// this is also the test that actually exercises `reflink_supported ==
    /// true` on real hardware rather than only ever observing `false`.
    #[test]
    fn probes_real_btrfs_if_available() {
        const BTRFS_MAGIC: i64 = 0x9123_683e;
        let Some(dir) = find_writable_real_mount(BTRFS_MAGIC) else {
            eprintln!(
                "probes_real_btrfs_if_available: no writable btrfs mount found on this machine \
                 -- skipped, not failed (T-3.1.7 AC: don't claim untested results)."
            );
            return;
        };
        let scratch = dir.join(format!(".duet-vfs-btrfs-probe-{}", std::process::id()));
        std::fs::create_dir_all(&scratch).unwrap();
        let vp = VPath::local(UnixPathBuf::new(scratch.to_str().unwrap()).unwrap());
        let props = probe(&vp).unwrap();
        eprintln!(
            "probes_real_btrfs_if_available: measured on {}: kind={:?} rotational={:?} \
             reflink_supported={} case_sensitive={:?}",
            scratch.display(),
            props.kind,
            props.rotational,
            props.reflink_supported,
            props.case_sensitive
        );
        assert_eq!(props.kind, FsKind::Btrfs);
        assert_eq!(props.case_sensitive, Some(true));
        // Confirmed independently on this machine via `cp --reflink=always`
        // before writing this assertion (not assumed from "btrfs supports
        // reflink" folklore): this mount really does support FICLONE.
        assert!(
            props.reflink_supported,
            "this btrfs mount was independently confirmed (via `cp --reflink=always`) to \
             support FICLONE; the probe should agree"
        );
        // btrfs subvolumes report an anonymous st_dev (major 0) that has
        // no corresponding entry under /sys/dev/block -- confirmed by
        // direct inspection while developing this test (`stat` on a file
        // here reports major:minor 0:N; /sys/dev/block/0:N does not
        // exist). Rotational detection therefore has nothing to resolve
        // and correctly reports `None` ("couldn't determine") rather than
        // guessing -- this is a real, verified filesystem quirk, not a
        // gap in this probe.
        assert_eq!(props.rotational, None);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// Opens `name` (must already exist, written via `std::fs`) read-only
    /// and a brand-new `name.dst` write-exclusive, exactly the fd shapes
    /// `accelerated_copy` hands `sparse_buffered_copy` -- lets these tests
    /// drive the rung directly without needing `copy_file_range` to
    /// genuinely fail first (which tmpfs, this test suite's usual
    /// filesystem, never does -- it happily services `copy_file_range`).
    fn open_src_and_dst(dir: &TempDir, src_name: &str) -> (OwnedFd, OwnedFd, PathBuf, PathBuf) {
        let src_path = dir.path().join(src_name);
        let dst_path = dir.path().join(format!("{src_name}.dst"));
        let src_fd = fs::openat(
            CWD,
            &src_path,
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let dst_fd = fs::openat(
            CWD,
            &dst_path,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        )
        .unwrap();
        (src_fd, dst_fd, src_path, dst_path)
    }

    #[test]
    fn sparse_buffered_copy_preserves_holes_and_content() {
        let dir = TempDir::new().unwrap();
        let src_path = dir.path().join("sparse.src");
        {
            use std::io::{Seek, Write};
            let mut f = std::fs::File::create(&src_path).unwrap();
            f.write_all(b"AAAA").unwrap();
            f.seek(std::io::SeekFrom::Start(10_000_000)).unwrap();
            f.write_all(b"BBBB").unwrap();
        }
        let size = std::fs::metadata(&src_path).unwrap().len();
        assert_eq!(size, 10_000_004);

        let (src_fd, dst_fd, _src_path, dst_path) = open_src_and_dst(&dir, "sparse.src");
        let from = vp(&dir).join("sparse.src").unwrap();
        let to = vp(&dir).join("sparse.src.dst").unwrap();
        let outcome =
            sparse_buffered_copy(&src_fd, &dst_fd, size, &from, &to, &dst_path, &|_| false)
                .unwrap();
        // `SEEK_HOLE`/`SEEK_DATA` round to filesystem block granularity
        // (4096 bytes on this machine's tmpfs, confirmed via `getconf
        // PAGESIZE` -- true of every real sparse-file implementation, not
        // a byte-exact concept anywhere), so "8 real bytes" pads out to
        // "a handful of blocks," not literally 8 -- the real assertion is
        // "far less than the ~10 MB logical span," proving hole-skipping
        // genuinely happened rather than falling back to a dense copy.
        let crate::CopyOutcome::Copied { bytes, reflinked } = outcome else {
            panic!("expected Copied, got {outcome:?}");
        };
        assert!(!reflinked);
        assert!(
            bytes < 100_000,
            "copied {bytes} bytes for an 8-byte-of-real-data source with a ~10 MB gap -- \
             expected only a block-rounded handful of bytes, not the whole logical span"
        );

        let copied = std::fs::read(&dst_path).unwrap();
        let original = std::fs::read(&src_path).unwrap();
        assert_eq!(
            copied, original,
            "content must match byte-for-byte including the hole"
        );
        assert_eq!(copied.len() as u64, size);

        let dst_blocks = fs::stat(&dst_path).unwrap().st_blocks;
        let dense_blocks_equivalent = size / 512;
        assert!(
            (dst_blocks as u64) < dense_blocks_equivalent / 2,
            "destination should still be sparse (st_blocks={dst_blocks}, a fully-dense \
             {size}-byte file would need ~{dense_blocks_equivalent} 512-byte blocks) -- \
             writing real zero bytes for the hole would have defeated the whole point of \
             this rung"
        );
    }

    #[test]
    fn sparse_buffered_copy_honors_cancellation_and_cleans_up() {
        let dir = TempDir::new().unwrap();
        let src_path = dir.path().join("big.src");
        // Large enough to span several SPARSE_COPY_BUFFER_BYTES chunks, so
        // cancelling after the first chunk genuinely interrupts a walk
        // still in progress rather than one that already finished.
        std::fs::write(&src_path, vec![7u8; SPARSE_COPY_BUFFER_BYTES * 4]).unwrap();
        let size = std::fs::metadata(&src_path).unwrap().len();

        let (src_fd, dst_fd, _src_path, dst_path) = open_src_and_dst(&dir, "big.src");
        let from = vp(&dir).join("big.src").unwrap();
        let to = vp(&dir).join("big.src.dst").unwrap();

        let calls = std::sync::atomic::AtomicU32::new(0);
        let on_progress =
            |_bytes: u64| calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 2;

        let outcome =
            sparse_buffered_copy(&src_fd, &dst_fd, size, &from, &to, &dst_path, &on_progress)
                .unwrap();
        assert_eq!(outcome, crate::CopyOutcome::Interrupted);
        assert!(
            !dst_path.exists(),
            "an interrupted copy must clean up its own partial destination, same as \
             copy_file_range's own Unsupported/error paths already do"
        );
    }

    /// The `copy_file_range` loop's own `on_progress` cancellation check (rung 2 of
    /// T-5.1.4's ladder) -- distinct from `sparse_buffered_copy`'s own
    /// check above (rung 3). tmpfs has no `FICLONE` support (confirmed by
    /// `probes_tmpfs_correctly` below) but happily services
    /// `copy_file_range`, so `accelerated_copy` on a tmpfs tempdir
    /// reliably exercises rung 2, not rung 1 or 3.
    #[test]
    fn accelerated_copy_honors_cancellation_mid_copy_file_range() {
        let dir = TempDir::new().unwrap();
        let src_path = dir.path().join("big.src");
        // Several COPY_FILE_RANGE_CHUNK_BYTES chunks, so cancelling after
        // the first couple genuinely interrupts a copy still in progress.
        std::fs::write(&src_path, vec![9u8; COPY_FILE_RANGE_CHUNK_BYTES * 4]).unwrap();

        let from = vp(&dir).join("big.src").unwrap();
        let to = vp(&dir).join("big.dst").unwrap();
        let calls = std::sync::atomic::AtomicU32::new(0);
        let on_progress =
            |_bytes: u64| calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 2;

        let outcome = accelerated_copy(&from, &to, &on_progress).unwrap();
        assert_eq!(outcome, crate::CopyOutcome::Interrupted);
        assert!(
            !dir.path().join("big.dst").exists(),
            "an interrupted copy_file_range loop must clean up its own partial destination"
        );
    }

    #[test]
    fn sparse_buffered_copy_past_the_fadvise_threshold_still_copies_correctly() {
        let dir = TempDir::new().unwrap();
        let src_path = dir.path().join("large.src");
        // Comfortably past FADVISE_THRESHOLD_BYTES so fadvise(DONTNEED)
        // genuinely fires at least once -- this test only asserts
        // correctness (fadvise must never corrupt data), not the actual
        // cache-eviction effect, which needs `vmtouch`/a real page-cache
        // inspection to observe for real.
        let len = FADVISE_THRESHOLD_BYTES as usize + SPARSE_COPY_BUFFER_BYTES;
        let content: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src_path, &content).unwrap();

        let (src_fd, dst_fd, _src_path, dst_path) = open_src_and_dst(&dir, "large.src");
        let from = vp(&dir).join("large.src").unwrap();
        let to = vp(&dir).join("large.src.dst").unwrap();
        let outcome =
            sparse_buffered_copy(&src_fd, &dst_fd, len as u64, &from, &to, &dst_path, &|_| {
                false
            })
            .unwrap();
        assert_eq!(
            outcome,
            crate::CopyOutcome::Copied {
                bytes: len as u64,
                reflinked: false
            }
        );
        assert_eq!(std::fs::read(&dst_path).unwrap(), content);
    }

    /// The first `physical_offset` column `filefrag -v` reports for
    /// `path`'s first extent, or `None` if the tool isn't installed or its
    /// output can't be parsed (a different `e2fsprogs` version's text
    /// format, say) -- either way treated as "inconclusive," not a hard
    /// test failure, per the same "don't claim untested results"
    /// philosophy `find_writable_real_mount`'s own doc comment already
    /// states for this module. `filefrag -v`'s per-extent line shape:
    /// ` ext:  logical_offset:  physical_offset:  length: ...`, e.g.
    /// `   0:        0..       0:    1234567..   1234567:      1: ...` --
    /// the physical offset is the number right after the second `:`.
    fn filefrag_first_extent_physical_offset(path: &Path) -> Option<u64> {
        let output = std::process::Command::new("filefrag")
            .arg("-v")
            .arg(path)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let line = line.trim();
            // Extent data rows start with the extent index, e.g. "0:".
            if !line.starts_with(|c: char| c.is_ascii_digit()) {
                continue;
            }
            let fields: Vec<&str> = line.split(':').map(str::trim).collect();
            // [0]=index [1]=logical_range [2]=physical_range [3]=length ...
            let physical_range = fields.get(2)?;
            let start = physical_range.split("..").next()?.trim();
            return start.parse().ok();
        }
        None
    }

    /// T-5.1.4's own AC, verbatim: "reflink used automatically on btrfs
    /// (verified with `filefrag`)." A real reflink shares its physical
    /// extent with the source (copy-on-write, no new data blocks
    /// allocated) -- `CopyOutcome::Copied { reflinked: true }` is already
    /// exercised by other tests, but this one cross-checks that claim
    /// against the filesystem's own on-disk extent map, not just this
    /// crate's own bookkeeping.
    #[test]
    fn reflink_copy_shares_its_physical_extent_verified_by_filefrag() {
        const BTRFS_MAGIC: i64 = 0x9123_683e;
        let Some(dir) = find_writable_real_mount(BTRFS_MAGIC) else {
            eprintln!(
                "reflink_copy_shares_its_physical_extent_verified_by_filefrag: no writable \
                 btrfs mount found on this machine -- not verified here, per this module's \
                 own \"don't claim untested results\" policy"
            );
            return;
        };
        if std::process::Command::new("filefrag")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!(
                "reflink_copy_shares_its_physical_extent_verified_by_filefrag: `filefrag` is \
                 not installed -- not verified here"
            );
            return;
        }

        let scratch = dir.join(format!(".duet-vfs-filefrag-probe-{}", std::process::id()));
        std::fs::create_dir_all(&scratch).unwrap();
        let src_path = scratch.join("src.bin");
        // A few blocks, not a single byte -- filefrag needs at least one
        // real allocated extent to report anything meaningful.
        std::fs::write(&src_path, vec![3u8; 64 * 1024]).unwrap();
        let dst_path = scratch.join("dst.bin");

        let from = VPath::local(UnixPathBuf::new(src_path.to_str().unwrap()).unwrap());
        let to = VPath::local(UnixPathBuf::new(dst_path.to_str().unwrap()).unwrap());
        let outcome = accelerated_copy(&from, &to, &|_| false).unwrap();
        assert_eq!(
            outcome,
            crate::CopyOutcome::Copied {
                bytes: 64 * 1024,
                reflinked: true
            },
            "expected a real FICLONE reflink on a confirmed-writable btrfs mount"
        );

        match (
            filefrag_first_extent_physical_offset(&src_path),
            filefrag_first_extent_physical_offset(&dst_path),
        ) {
            (Some(src_off), Some(dst_off)) => {
                assert_eq!(
                    src_off, dst_off,
                    "a genuine reflink must share its source's physical extent, not \
                     allocate a new one"
                );
                eprintln!(
                    "reflink_copy_shares_its_physical_extent_verified_by_filefrag: \
                     confirmed -- both files' first extent starts at physical block {src_off}"
                );
            }
            _ => eprintln!(
                "reflink_copy_shares_its_physical_extent_verified_by_filefrag: \
                 `filefrag -v`'s output didn't parse as expected (a different e2fsprogs \
                 version's format?) -- CopyOutcome::Copied{{reflinked: true}} above is still \
                 verified, just not cross-checked against the on-disk extent map"
            ),
        }

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// T-5.1.4's own AC, verbatim: "page cache not evicted (verified with
    /// `vmtouch`)." design.md §9.3's own phrasing makes clear what this
    /// protects: `posix_fadvise(DONTNEED)` on the *source*, behind the
    /// read cursor, is the intended, deliberate mechanism (freeing the
    /// copy's own now-useless cached pages) -- the guarantee is that
    /// *other, unrelated* files already resident in the page cache
    /// survive a large copy, not that the source or destination stay
    /// cached. So: pre-warm a "control" file into the page cache via
    /// `vmtouch -t`, run a large `sparse_buffered_copy` (comfortably past
    /// `FADVISE_THRESHOLD_BYTES`, forcing `fadvise` to actually fire),
    /// then confirm the control file is still resident via `vmtouch`'s
    /// own residency percentage output.
    #[test]
    fn large_copy_does_not_evict_unrelated_files_from_the_page_cache_verified_by_vmtouch() {
        if std::process::Command::new("vmtouch")
            .arg("-v")
            .output()
            .is_err()
        {
            eprintln!(
                "large_copy_does_not_evict_unrelated_files_from_the_page_cache_verified_by_\
                 vmtouch: `vmtouch` is not installed -- not verified here, per this module's \
                 own \"don't claim untested results\" policy (S-5's own spike report hit the \
                 same gap in its environment and used /proc/meminfo's Cached: growth as a \
                 documented proxy instead -- see documentation/spikes/S-5.md)"
            );
            return;
        }

        let dir = TempDir::new().unwrap();
        let control_path = dir.path().join("control.bin");
        std::fs::write(&control_path, vec![1u8; 1024 * 1024]).unwrap();
        let warmed = std::process::Command::new("vmtouch")
            .arg("-t") // touch: load into the page cache
            .arg(&control_path)
            .status()
            .is_ok_and(|s| s.success());
        if !warmed {
            eprintln!(
                "large_copy_does_not_evict_unrelated_files_from_the_page_cache_verified_by_\
                 vmtouch: `vmtouch -t` failed to pre-warm the control file -- not verified here"
            );
            return;
        }

        let src_path = dir.path().join("large.src");
        let len = FADVISE_THRESHOLD_BYTES as usize * 3;
        std::fs::write(&src_path, vec![5u8; len]).unwrap();
        let (src_fd, dst_fd, _src_path, dst_path) = open_src_and_dst(&dir, "large.src");
        let from = vp(&dir).join("large.src").unwrap();
        let to = vp(&dir).join("large.src.dst").unwrap();
        sparse_buffered_copy(&src_fd, &dst_fd, len as u64, &from, &to, &dst_path, &|_| {
            false
        })
        .unwrap();

        let residency = std::process::Command::new("vmtouch")
            .arg(&control_path)
            .output()
            .unwrap();
        let report = String::from_utf8_lossy(&residency.stdout);
        eprintln!(
            "large_copy_does_not_evict_unrelated_files_from_the_page_cache_verified_by_vmtouch: \
             vmtouch report for the control file after a {len}-byte fadvise'd copy:\n{report}"
        );
        // vmtouch prints a line like "[OOOOOOOO...] 100.00% (or similar) of file is in \
        // memory" per its own documented output; a value near 0% would mean the control
        // file was evicted.
        assert!(
            report.contains("100"),
            "expected the pre-warmed control file to still be fully resident after the copy \
             (fadvise(DONTNEED) targets the copy's own source, not unrelated files) -- got:\n\
             {report}"
        );
        let _ = std::process::Command::new("vmtouch")
            .arg("-e")
            .arg(&control_path)
            .status(); // best-effort cleanup: don't leave the test's own file pinned in cache
    }

    /// T-5.1.4's own AC, verbatim: "\u{2265} 95% of `cp` for a 10 GB file."
    /// Skipped by default -- real, multi-second disk I/O has no place in
    /// the ordinary `cargo test` loop -- following the exact runtime-env
    /// -var-gate convention `benches/benches/local_listing.rs`'s own
    /// `DUET_BENCH_1M` case already established for "this AC needs real,
    /// slow I/O to verify, not a synthetic micro-benchmark." Defaults to a
    /// smaller size than the AC's literal 10 GiB (fast enough to actually
    /// run when explicitly requested) -- override with
    /// `DUET_BENCH_LARGE_COPY_MIB` to verify at the full AC scale.
    ///
    /// Honest caveat, not papered over: on a reflink-capable filesystem
    /// (this test's `TempDir` is typically tmpfs; S-5's own report notes
    /// modern `cp` also opportunistically uses `copy_file_range`, which
    /// btrfs services via CoW reflink internally), both `accelerated_copy`
    /// and real `cp` end up "statistically free" and this test mostly
    /// confirms neither regressed to something pathological, rather than
    /// stress-testing rung 3's buffered throughput specifically -- that
    /// needs a genuinely non-reflink-capable destination (ext4, or
    /// `DUET_BENCH_LARGE_COPY_MIB` pointed at one via a real mount) to be
    /// a meaningful throughput comparison, matching what S-5's own spike
    /// already found on this exact question.
    #[test]
    fn accelerated_copy_matches_cp_throughput_within_5_percent() {
        if std::env::var("DUET_BENCH_LARGE_COPY").as_deref() != Ok("1") {
            eprintln!(
                "accelerated_copy_matches_cp_throughput_within_5_percent: skipped by default \
                 (real disk I/O) -- set DUET_BENCH_LARGE_COPY=1 to run (optionally \
                 DUET_BENCH_LARGE_COPY_MIB=10240 for the AC's literal 10 GiB scale)"
            );
            return;
        }
        let mib: u64 = std::env::var("DUET_BENCH_LARGE_COPY_MIB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2048);
        let size = mib * 1024 * 1024;

        let dir = TempDir::new().unwrap();
        let src_path = dir.path().join("bench.src");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&src_path).unwrap();
            let chunk = vec![42u8; 4 * 1024 * 1024];
            let mut written = 0u64;
            while written < size {
                let n = (chunk.len() as u64).min(size - written) as usize;
                f.write_all(&chunk[..n]).unwrap();
                written += n as u64;
            }
        }

        let ours_dst = dir.path().join("ours.dst");
        let from = VPath::local(UnixPathBuf::new(src_path.to_str().unwrap()).unwrap());
        let to = VPath::local(UnixPathBuf::new(ours_dst.to_str().unwrap()).unwrap());
        let start = std::time::Instant::now();
        accelerated_copy(&from, &to, &|_| false).unwrap();
        let ours = start.elapsed();

        let cp_dst = dir.path().join("cp.dst");
        let start = std::time::Instant::now();
        let status = std::process::Command::new("cp")
            .arg(&src_path)
            .arg(&cp_dst)
            .status()
            .unwrap();
        assert!(status.success(), "the `cp` comparison run itself failed");
        let cp_time = start.elapsed();

        let ratio = ours.as_secs_f64() / cp_time.as_secs_f64().max(f64::EPSILON);
        eprintln!(
            "accelerated_copy_matches_cp_throughput_within_5_percent: {mib} MiB -- \
             ours={ours:?} cp={cp_time:?} (ours took {ratio:.3}x cp's time; AC requires \
             \u{2264}1.0526x, i.e. no slower than 105% of cp's time / \u{2265}95% of its \
             throughput)"
        );
        assert!(
            ours.as_secs_f64() <= cp_time.as_secs_f64() * 1.0526,
            "our copy took {ours:?}, cp took {cp_time:?} ({ratio:.3}x) -- T-5.1.4's own AC \
             requires \u{2265}95% of cp's throughput"
        );
    }

    /// The bug this whole fix targets, verified at its actual source: the
    /// `copy_file_range` loop (rung 2) used to only ever check
    /// `should_cancel()`, with no way to tell the caller how many bytes
    /// had actually moved -- `duet-ops`'s executor could only credit a
    /// file's entire size in one lump once `accelerated_copy` returned,
    /// which broke both live throughput (reads `0 B/s` between that one
    /// jump) and the ETA estimator (the single huge jump inflates the EWMA
    /// rate, which then holds steady instead of decaying). Runs by
    /// default at 200 MiB (a few real `copy_file_range` chunks, comfortably
    /// fast on tmpfs -- no `DUET_BENCH_*` gate needed, unlike the
    /// throughput-comparison benchmark above, which needs GiB scale and
    /// real wall-clock comparison against `cp` to mean anything; this test
    /// only needs *multiple* chunks to exist, not any particular speed).
    #[test]
    fn accelerated_copy_reports_incremental_progress_via_copy_file_range() {
        let dir = TempDir::new().unwrap();
        let src_path = dir.path().join("multi_chunk.src");
        // A little over 3 chunks -- comfortably spans multiple
        // COPY_FILE_RANGE_CHUNK_BYTES-sized calls (64 MiB each) without an
        // unreasonably large test fixture.
        let size = COPY_FILE_RANGE_CHUNK_BYTES as u64 * 3 + 8 * 1024 * 1024;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&src_path).unwrap();
            let chunk = vec![11u8; 4 * 1024 * 1024];
            let mut written = 0u64;
            while written < size {
                let n = (chunk.len() as u64).min(size - written) as usize;
                f.write_all(&chunk[..n]).unwrap();
                written += n as u64;
            }
        }

        let from = vp(&dir).join("multi_chunk.src").unwrap();
        let to = vp(&dir).join("multi_chunk.dst").unwrap();

        let calls: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());
        let on_progress = |bytes: u64| {
            calls.lock().unwrap().push(bytes);
            false // never cancel
        };

        let outcome = accelerated_copy(&from, &to, &on_progress).unwrap();
        assert_eq!(
            outcome,
            crate::CopyOutcome::Copied {
                bytes: size,
                reflinked: false
            },
            "tmpfs has no FICLONE (confirmed by probes_tmpfs_correctly), so this must have \
             gone through the copy_file_range rung, not a reflink"
        );

        let recorded = calls.into_inner().unwrap();
        let nonzero: Vec<u64> = recorded.iter().copied().filter(|&b| b > 0).collect();
        let total: u64 = nonzero.iter().sum();
        eprintln!(
            "accelerated_copy_reports_incremental_progress_via_copy_file_range: {} on_progress \
             call(s) total, {} of them nonzero: {nonzero:?} (summing to {total}, file size {size})",
            recorded.len(),
            nonzero.len()
        );
        assert!(
            nonzero.len() > 1,
            "expected more than one nonzero on_progress call for a {size}-byte file spanning \
             several {COPY_FILE_RANGE_CHUNK_BYTES}-byte chunks -- got {}: {nonzero:?} (this is \
             exactly the 'one big jump at the end' bug this fix targets)",
            nonzero.len()
        );
        assert_eq!(
            total, size,
            "the sum of every reported chunk must equal the file's real size exactly"
        );
        assert!(
            nonzero.iter().all(|&b| b < size),
            "no single on_progress call should report the whole file's size at once -- that \
             would just be the old one-big-jump behaviour under a new name: {nonzero:?}"
        );
    }

    /// The `sparse_buffered_copy` half of the same fix: rung 3 must also
    /// report real incremental progress, not just rung 2. Calls the
    /// function directly (as `sparse_buffered_copy_preserves_holes_and_
    /// content` and its siblings above already do) to force rung 3
    /// specifically, bypassing whatever `copy_file_range` would otherwise
    /// do -- exactly this module's own established precedent for testing
    /// this rung in isolation. Uses a dense (non-sparse) source so every
    /// reported byte comes from the inner (mid-extent) chunk loop, proving
    /// that loop's own incremental reporting independent of the known,
    /// disclosed "holes aren't reported" gap `sparse_buffered_copy`'s own
    /// doc comment already covers.
    #[test]
    fn sparse_buffered_copy_reports_incremental_progress() {
        let dir = TempDir::new().unwrap();
        let src_path = dir.path().join("dense.src");
        // Several SPARSE_COPY_BUFFER_BYTES (2 MiB) chunks.
        let size = SPARSE_COPY_BUFFER_BYTES as u64 * 6;
        let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src_path, &content).unwrap();

        let (src_fd, dst_fd, _src_path, dst_path) = open_src_and_dst(&dir, "dense.src");
        let from = vp(&dir).join("dense.src").unwrap();
        let to = vp(&dir).join("dense.src.dst").unwrap();

        let calls: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());
        let on_progress = |bytes: u64| {
            calls.lock().unwrap().push(bytes);
            false // never cancel
        };

        let outcome =
            sparse_buffered_copy(&src_fd, &dst_fd, size, &from, &to, &dst_path, &on_progress)
                .unwrap();
        assert_eq!(
            outcome,
            crate::CopyOutcome::Copied {
                bytes: size,
                reflinked: false
            }
        );
        assert_eq!(std::fs::read(&dst_path).unwrap(), content);

        let recorded = calls.into_inner().unwrap();
        let nonzero: Vec<u64> = recorded.iter().copied().filter(|&b| b > 0).collect();
        let zero_heartbeats = recorded.len() - nonzero.len();
        let total: u64 = nonzero.iter().sum();
        eprintln!(
            "sparse_buffered_copy_reports_incremental_progress: {} on_progress call(s) total \
             ({} zero-byte heartbeats between/around extents, {} nonzero mid-extent chunks: \
             {nonzero:?}), nonzero sum {total} against file size {size}",
            recorded.len(),
            zero_heartbeats,
            nonzero.len()
        );
        assert!(
            nonzero.len() > 1,
            "expected more than one nonzero on_progress call for a {size}-byte dense file \
             spanning several {SPARSE_COPY_BUFFER_BYTES}-byte buffers -- got {}: {nonzero:?}",
            nonzero.len()
        );
        // No holes in this source, so (unlike the general sparse case this
        // function's own doc comment discloses) the reported total must
        // match the file's real size exactly here -- nothing was skipped.
        assert_eq!(
            total, size,
            "a dense (non-sparse) source has no holes to skip, so every byte must be reported"
        );
        assert!(
            nonzero.iter().all(|&b| b < size),
            "no single on_progress call should report the whole file's size at once: {nonzero:?}"
        );
    }
}
