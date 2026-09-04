// SPDX-License-Identifier: MIT
//! T-5.3.1: the full freedesktop trash-spec implementation (design.md
//! §9.10, FR-CFG-07) — `$topdir/.Trash-$uid` for a target on a different
//! filesystem than `$XDG_DATA_HOME`, `.trashinfo` sidecar metadata, and the
//! two-method per-mount trash-directory resolution the spec defines. This
//! module owns only the *pure decision*: given a target's real path and
//! `$XDG_DATA_HOME`, where does its trashed content go, and what does its
//! `.trashinfo` sidecar say — as plain `std::path::PathBuf`/`String` values,
//! with no `Step`/`Plan`/`FileSystem` involved at all. `duet_ops::deleter`
//! is the one caller: it turns [`resolve_trash_destination`]'s answer into
//! a `Step::WriteTrashInfo` + `Step::Rename` pair per target (see that
//! module's own doc comment for why the actual filesystem mutation stays
//! there, journaled, rather than happening inside this crate).
//!
//! # Why plan-time name resolution, not the executor's live `AutoRename`
//!
//! The spec requires a `.trashinfo` file whose name exactly matches its
//! paired content file inside `$trash/files/` — so if two targets in one
//! job both happen to be named `a.txt`, the second one needs its
//! `.trashinfo` written for whatever collision-free name it actually lands
//! at (`a (2).txt`), not for `a.txt`. `duet_ops::executor`'s own
//! `ConflictPolicy::AutoRename` resolves that name live, at *execution*
//! time, with no channel to report the chosen name back to anything that
//! could still act on it (`StepOutcome::Succeeded` carries no data).
//! Reusing it here would mean `.trashinfo` could never reliably know what
//! name to target.
//!
//! [`resolve_trash_destination`] resolves the collision-free name
//! *up front*, at plan-build time, via [`unique_trash_name`]'s own
//! `fs.stat`-probing loop — the same shape `executor::auto_rename_target`
//! already uses, just called earlier, by the planner rather than the
//! executor. `duet_ops::deleter` bakes the exact resolved name into both
//! the `Step::WriteTrashInfo`'s `info_path` and the `Step::Rename`'s
//! `dest`, so the two can never disagree about the name — and the
//! `Rename`'s own conflict policy needs no live resolution at all
//! (`Some(ConflictPolicy::Abort)`, a paranoid backstop for the plan-time-
//! to-execution-time TOCTOU window every other `AutoRename` use already
//! has and already mitigates the same way: fail loudly rather than
//! silently pick a second name `.trashinfo` was never written for).
//!
//! # T-5.3.2 phase 1: the read side
//!
//! [`list_trash_entries`] is the inverse of everything above: given
//! `$XDG_DATA_HOME`, enumerate every `.trashinfo` sidecar this module (or
//! any other freedesktop-spec-compliant tool) has ever written, parsing
//! each one's `Path=`/`DeletionDate=` fields back into a real
//! [`TrashEntry`] — percent-decoding, and undoing [`local_civil_time`] via
//! the standard `mktime(3)` inverse. `duet_ops::trash_restore` (T-5.3.2's
//! own planner module) is the one caller, turning each entry into a
//! restore or permanent-purge `Plan`.
//!
//! # T-5.3.2 phase 2: per-mount trash discovery
//!
//! Phase 1 left a real, disclosed gap: [`list_trash_entries`] only ever
//! scanned `$XDG_DATA_HOME/Trash`, so a target trashed from a different
//! mounted filesystem (its content and `.trashinfo` correctly written by
//! [`resolve_trash_destination`] to `$topdir/.Trash{,-$uid}`, per the
//! write side above) never showed up in the browser at all — confirmed
//! live: the write path was always correct, only discovery was missing.
//!
//! [`list_trash_entries`] now also walks `/proc/self/mountinfo`
//! ([`parse_mountinfo`] is the pure, independently-testable line parser),
//! filters out pseudo/virtual filesystem types that could never hold a
//! per-mount trash ([`is_probeable_fstype`]), and probes each surviving
//! mount point for an *already-existing* trash root
//! ([`list_entries_from_candidate_topdirs`], reusing
//! [`find_method_one_root`]/[`find_method_two_root`] — the read-only,
//! never-creates-anything counterparts to [`try_method_one`]/
//! [`try_method_two`]) — merging whatever it finds with home trash's own
//! entries. A candidate whose device matches `$XDG_DATA_HOME`'s own (a
//! bind mount, or `$XDG_DATA_HOME` itself showing up as a "mount point")
//! is skipped, via [`dedup_and_exclude_home_dev`], so home-trash entries
//! are never double-counted; candidates that share a device with each
//! other (two mount-point paths onto the same underlying filesystem) are
//! also deduplicated there. A failure reading `/proc/self/mountinfo`
//! itself degrades to home-trash-only rather than failing the whole call
//! — home trash always working is the load-bearing guarantee, per-mount
//! discovery is enhancement on top.
//!
//! This is a one-shot scan per [`list_trash_entries`] call, same as phase
//! 1 — no live mount-table tracking (that is design.md's separate,
//! unbuilt "Mounts" concern, T-6.1.1).
//!
//! The mount-table read is the one piece of global machine state in this
//! module's read side, and the only one a caller's `$XDG_DATA_HOME`
//! redirect can't reach — so [`list_trash_entries_with_mounts`] takes the
//! candidate source as an explicit [`MountScan`] (`System`, the real table,
//! or `Explicit(paths)`, which touches nothing outside the paths named).
//! [`list_trash_entries`] is simply the `System` shorthand. `duet-ui`'s
//! tests are the motivating caller: with only the XDG redirect, every
//! trash-browser test on a machine with a real trashed file on a second
//! drive found that file mixed into its own fixtures.
//!
//! **A malformed `.trashinfo` is skipped, not propagated as an error for
//! the whole listing.** A hand-edited, truncated, or third-party-tool-
//! written sidecar that doesn't parse per this module's own writer's shape
//! must not hide every *other*, legitimate entry in the same directory —
//! [`list_trash_root`] treats a per-entry parse failure as "not a trash
//! entry, move on," the same "tolerate garbage, don't propagate it"
//! philosophy `try_method_one`/`try_method_two`'s own "fall through, don't
//! error" convention already established for this module, just applied to
//! parsing instead of directory-safety checks.
//!
//! # Local time without a date/time crate dependency
//!
//! `.trashinfo`'s `DeletionDate` must be local time, no timezone suffix.
//! This crate has no `chrono`/`time`/`jiff` dependency (design.md §7.5's
//! "additions earn their keep" policy, and `duet-platform` is otherwise
//! dependency-free) — correctly converting a Unix timestamp to local civil
//! time needs the system timezone database, which none of this crate's
//! existing tools (`rustix`, hand-rolled Howard-Hinnant civil-time algebra
//! like `duet-ui::file_table::civil_from_unix` already uses for UTC) can
//! provide on their own. [`local_civil_time`] instead calls the C library's
//! `localtime_r(3)` directly via a small hand-written `extern "C"` binding
//! — no `libc` crate needed, since every Linux binary already links against
//! the system libc, and Duet is Linux-only (this whole module already
//! assumes Linux-specific syscalls throughout). The `struct tm` layout
//! mirrored here (`sec/min/hour/mday/mon/year/wday/yday/isdst` plus the
//! glibc/musl `tm_gmtoff`/`tm_zone` extensions, in that order) is the same
//! on both major Linux libc implementations.

use std::collections::HashSet;
use std::ffi::c_char;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rustix::fs::{self, AtFlags, CWD, FileType, Mode};
use rustix::io::Errno;

/// Tracks trash-content destinations already claimed by an earlier
/// [`resolve_trash_destination`]/[`resolve_trash_destination_at`] call
/// *within the same planning pass*.
///
/// This exists because [`unique_trash_name`]'s own collision probe only
/// sees what already exists on disk right now -- and two targets trashed
/// together in one job haven't actually moved anything yet by the time the
/// second one is planned (planning and execution are separate phases; see
/// `duet_ops::deleter`'s own module doc comment). Without this, two
/// same-named targets in one job would both resolve to the identical
/// `files/<name>` destination (each one's own probe finding nothing on
/// disk yet), and the second target's `Step::Rename` would collide with
/// the first's at execution time -- exactly the freedesktop-spec-mandated
/// disambiguation this module exists to get right, silently defeated by
/// planning-vs-execution timing.
///
/// A caller building one job's worth of trash steps constructs one
/// `TrashReservations` and passes the same instance to every target's
/// resolution call; a caller resolving destinations independently (e.g. a
/// one-off, single-target trash) can pass a fresh
/// [`TrashReservations::new`] each time.
#[derive(Debug, Default)]
pub struct TrashReservations(HashSet<PathBuf>);

impl TrashReservations {
    pub fn new() -> Self {
        Self::default()
    }
}

/// A target's resolved trash destination — [`resolve_trash_destination`]'s
/// whole answer. `duet_ops::deleter` turns this directly into a
/// `Step::WriteTrashInfo { info_path, content: trashinfo }` followed by a
/// `Step::Rename { dest: content_path, .. }`, in that order (see the module
/// doc comment's "Why plan-time name resolution" section for why the name
/// in both paths is guaranteed to match).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTrash {
    /// Where the trashed content itself goes — `$trash/files/<name>`.
    pub content_path: PathBuf,
    /// Where the `.trashinfo` sidecar goes — `$trash/info/<name>.trashinfo`,
    /// the exact same `<name>` as `content_path`.
    pub info_path: PathBuf,
    /// The fully formatted `.trashinfo` file content (`[Trash Info]` header,
    /// `Path=`, `DeletionDate=`), ready to write verbatim.
    pub trashinfo: String,
}

/// Everything that can go wrong resolving a target's trash destination.
#[derive(Debug)]
pub enum TrashError {
    /// A `stat`/`mkdir`/`chmod` syscall failed. `path` is whichever path
    /// was being operated on when it happened.
    Io { path: PathBuf, source: io::Error },
    /// Neither method 1 (`$topdir/.Trash/$uid`, sticky bit set, not a
    /// symlink) nor method 2 (`$topdir/.Trash-$uid`) could be used for
    /// `topdir` — a read-only filesystem with no writable trash location
    /// at all, e.g. This is a genuine, surfaced failure per this
    /// codebase's "no silent failure" convention (never silently falls
    /// back to copying into the home trash instead).
    NoUsableTrash { topdir: PathBuf },
}

impl std::fmt::Display for TrashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrashError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            TrashError::NoUsableTrash { topdir } => write!(
                f,
                "{}: no usable trash directory (neither .Trash/$uid with the sticky bit set \
                 nor .Trash-$uid could be used)",
                topdir.display()
            ),
        }
    }
}

impl std::error::Error for TrashError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TrashError::Io { source, .. } => Some(source),
            TrashError::NoUsableTrash { .. } => None,
        }
    }
}

fn io_err(e: Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

fn io_err_at(path: &Path, e: Errno) -> TrashError {
    TrashError::Io {
        path: path.to_path_buf(),
        source: io_err(e),
    }
}

/// As [`io_err_at`], but for an already-converted `io::Error` (e.g. from
/// [`dev_of`]/[`topdir_of`], which already return `io::Result` themselves).
fn trash_io_err(path: &Path, e: io::Error) -> TrashError {
    TrashError::Io {
        path: path.to_path_buf(),
        source: e,
    }
}

/// Resolves `target`'s trash destination — see the module doc comment for
/// the full design. `target` must currently exist (its device is stat'd);
/// `xdg_data_home` is `duet_config::paths::xdg_data_home()`'s result,
/// passed in rather than resolved here since environment/`$HOME`
/// resolution is a `duet-config` concern, not this backend-agnostic
/// crate's (mirroring `duet_ops::deleter`'s own established boundary with
/// `duet-config`).
///
/// Creates whatever trash-root directories the resolution needs
/// (`$XDG_DATA_HOME/Trash/{files,info}` or `$topdir/.Trash/$uid/
/// {files,info}` or `$topdir/.Trash-$uid/{files,info}`) as a side effect —
/// this is a synchronous, blocking function (a handful of `stat`/`mkdir`/
/// `chmod` syscalls), meant to be called off the UI thread, the same way
/// every other blocking call this crate's callers make already is (see
/// `duet_ops::deleter::plan_delete`'s own doc comment: planning already
/// does inline blocking `FileSystem` calls in the same async context this
/// is called from).
///
/// # Errors
/// [`TrashError::Io`] for any failed syscall; [`TrashError::NoUsableTrash`]
/// if `target` is on a different filesystem than `xdg_data_home` and
/// neither per-mount trash method is usable there.
pub fn resolve_trash_destination(
    target: &Path,
    xdg_data_home: &Path,
    reservations: &mut TrashReservations,
) -> Result<ResolvedTrash, TrashError> {
    resolve_trash_destination_at(target, xdg_data_home, SystemTime::now(), reservations)
}

/// [`resolve_trash_destination`] with an explicit `DeletionDate` clock
/// source, for deterministic tests.
pub fn resolve_trash_destination_at(
    target: &Path,
    xdg_data_home: &Path,
    now: SystemTime,
    reservations: &mut TrashReservations,
) -> Result<ResolvedTrash, TrashError> {
    let location = resolve_trash_location(target, xdg_data_home)?;

    let original_name =
        target
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| TrashError::Io {
                path: target.to_path_buf(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "target has no file name"),
            })?;
    let unique_name = unique_trash_name(
        &location.files_dir,
        &location.info_dir,
        original_name,
        reservations,
    )
    .map_err(|e| TrashError::Io {
        path: location.files_dir.clone(),
        source: e,
    })?;

    let content_path = location.files_dir.join(&unique_name);
    let info_path = location.info_dir.join(format!("{unique_name}.trashinfo"));
    reservations.0.insert(content_path.clone());

    let path_field = match &location.topdir {
        // Home trash: absolute, percent-encoded original path.
        None => percent_encode_path(&target.to_string_lossy()),
        // Topdir trash: percent-encoded path *relative to topdir* -- an
        // absolute Path here is the spec's single biggest correctness
        // trap (see the module doc comment).
        Some(topdir) => {
            let relative = target.strip_prefix(topdir).unwrap_or(target);
            percent_encode_path(&relative.to_string_lossy())
        }
    };
    let civil = local_civil_time(unix_secs(now));
    let trashinfo = format_trashinfo(&path_field, civil);

    Ok(ResolvedTrash {
        content_path,
        info_path,
        trashinfo,
    })
}

/// The trash root a target should use, already created (`files`/`info`
/// subdirectories included) by the time this returns.
struct TrashLocation {
    files_dir: PathBuf,
    info_dir: PathBuf,
    /// `None` for home trash (`Path=` is absolute); `Some(topdir)` for a
    /// per-mount trash (`Path=` is relative to `topdir`).
    topdir: Option<PathBuf>,
}

fn resolve_trash_location(
    target: &Path,
    xdg_data_home: &Path,
) -> Result<TrashLocation, TrashError> {
    let target_dev = dev_of(target).map_err(|e| trash_io_err(target, e))?;

    std::fs::create_dir_all(xdg_data_home).map_err(|e| TrashError::Io {
        path: xdg_data_home.to_path_buf(),
        source: e,
    })?;
    let home_dev = dev_of(xdg_data_home).map_err(|e| trash_io_err(xdg_data_home, e))?;

    if target_dev == home_dev {
        let root = xdg_data_home.join("Trash");
        create_trash_root(&root)?;
        return Ok(TrashLocation {
            files_dir: root.join("files"),
            info_dir: root.join("info"),
            topdir: None,
        });
    }

    // A different filesystem: the trash itself must live there too, so a
    // trash "move" is always a same-device rename, never a cross-device
    // copy -- see the module doc comment's own note on why `duet_ops::
    // deleter` never needs a `CopyFile` fallback for trash.
    let topdir = topdir_of(target).map_err(|e| trash_io_err(target, e))?;
    let uid = rustix::process::getuid().as_raw();

    if let Some(root) = try_method_one(&topdir, uid) {
        return Ok(TrashLocation {
            files_dir: root.join("files"),
            info_dir: root.join("info"),
            topdir: Some(topdir),
        });
    }
    if let Some(root) = try_method_two(&topdir, uid) {
        return Ok(TrashLocation {
            files_dir: root.join("files"),
            info_dir: root.join("info"),
            topdir: Some(topdir),
        });
    }
    Err(TrashError::NoUsableTrash { topdir })
}

/// Walks up from `path`'s parent directory, comparing `st_dev` to `path`'s
/// own device, until it changes (or `/` is reached) -- the last directory
/// that still shares `path`'s device is its mount point ("topdir" in the
/// spec's terminology). No `/proc/self/mountinfo` parsing needed: a file
/// and its containing directory are always on the same device (a mount
/// happens at a directory boundary), so this `st_dev`-comparison walk finds
/// the boundary directly.
fn topdir_of(path: &Path) -> io::Result<PathBuf> {
    let target_dev = dev_of(path)?;
    let mut topdir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => return Ok(PathBuf::from("/")),
    };
    loop {
        if topdir == Path::new("/") {
            return Ok(topdir);
        }
        let Some(parent) = topdir.parent() else {
            return Ok(topdir);
        };
        let parent_dev = match dev_of(parent) {
            Ok(d) => d,
            // Can't stat further up (permission denied on some ancestor,
            // e.g.) -- treat what we've found so far as the boundary
            // rather than erroring the whole resolution over it.
            Err(_) => return Ok(topdir),
        };
        if parent_dev != target_dev {
            return Ok(topdir);
        }
        topdir = parent.to_path_buf();
    }
}

fn dev_of(path: &Path) -> io::Result<u64> {
    fs::statat(CWD, path, AtFlags::empty())
        .map(|st| st.st_dev)
        .map_err(io_err)
}

/// The core of "does this existing directory entry qualify as a per-user
/// trash root" per the spec: it must be a real directory (not a symlink),
/// owned by `uid`, and not group/other readable or writable. Shared by
/// [`existing_dir_is_safe_trash_root`] (the write side's "not there yet is
/// fine, the caller will create it" variant) and
/// [`find_existing_safe_trash_root`] (the read side's "must already exist"
/// variant) so the sticky-bit/symlink/ownership safety logic itself lives
/// in exactly one place.
fn is_safe_trash_root_dir(st: &fs::Stat, uid: u32) -> bool {
    FileType::from_raw_mode(st.st_mode) == FileType::Directory
        && st.st_uid == uid
        && st.st_mode & 0o077 == 0
}

/// Bytes that make an existing directory entry unsafe to reuse as a
/// per-user trash root -- see [`is_safe_trash_root_dir`]. Shared by
/// [`try_method_one`]/[`try_method_two`]'s "does an existing entry still
/// qualify" check.
fn existing_dir_is_safe_trash_root(path: &Path, uid: u32) -> bool {
    match fs::statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => is_safe_trash_root_dir(&st, uid),
        // Doesn't exist (or can't be stat'd) -- not "unsafe", just "not
        // there yet", which the caller creates fresh.
        Err(_) => true,
    }
}

/// The read-only counterpart to [`existing_dir_is_safe_trash_root`]: `true`
/// only if `path` *already exists* and qualifies -- used by
/// [`find_method_one_root`]/[`find_method_two_root`] (T-5.3.2 phase 2's
/// per-mount trash *browser*, which must never create anything -- see
/// those functions' own doc comments), where "doesn't exist" means "no
/// trash here to read", not "safe to go ahead and create".
fn find_existing_safe_trash_root(path: &Path, uid: u32) -> bool {
    match fs::statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => is_safe_trash_root_dir(&st, uid),
        Err(_) => false,
    }
}

const STICKY_BIT: u32 = 0o1000;

/// The shared "is `$topdir/.Trash` itself usable as method 1's base at
/// all" check: must exist, be a real directory (not a symlink -- a known
/// spec attack/misconfiguration vector), and have the sticky bit set.
/// `None` for any way this fails, letting both [`try_method_one`] (which
/// then creates `$topdir/.Trash/$uid` on demand) and [`find_method_one_root`]
/// (which only looks) fall through identically.
fn method_one_dot_trash_dir(topdir: &Path) -> Option<PathBuf> {
    let dot_trash = topdir.join(".Trash");
    let lst = fs::statat(CWD, &dot_trash, AtFlags::SYMLINK_NOFOLLOW).ok()?;
    if FileType::from_raw_mode(lst.st_mode) != FileType::Directory {
        return None; // missing, or not a directory at all
    }
    if lst.st_mode & STICKY_BIT == 0 {
        return None; // sticky bit not set
    }
    Some(dot_trash)
}

/// Method 1: `$topdir/.Trash/$uid`, only if `$topdir/.Trash` exists, is a
/// real directory (not a symlink), and has the sticky bit set. `None`
/// (fall through to method 2) for every way this can be unusable, per the
/// spec and this task's own "fall through, don't error" directive -- only
/// [`try_method_two`] failing is what produces a real, surfaced
/// [`TrashError`].
fn try_method_one(topdir: &Path, uid: u32) -> Option<PathBuf> {
    let dot_trash = method_one_dot_trash_dir(topdir)?;
    let user_dir = dot_trash.join(uid.to_string());
    if !existing_dir_is_safe_trash_root(&user_dir, uid) {
        return None;
    }
    create_trash_root(&user_dir).ok()?;
    Some(user_dir)
}

/// Method 2 (the fallback every real desktop actually uses in practice):
/// `$topdir/.Trash-$uid`, created at mode `0700` if missing.
fn try_method_two(topdir: &Path, uid: u32) -> Option<PathBuf> {
    let user_dir = topdir.join(format!(".Trash-{uid}"));
    if !existing_dir_is_safe_trash_root(&user_dir, uid) {
        return None;
    }
    create_trash_root(&user_dir).ok()?;
    Some(user_dir)
}

/// The read-only counterpart to [`try_method_one`]: `Some(root)` only if
/// `$topdir/.Trash/$uid` *already exists* and is safe -- never creates
/// `.Trash/$uid` (or anything else) as a side effect of merely looking,
/// unlike the write side. Used by [`list_entries_from_candidate_topdirs`],
/// T-5.3.2 phase 2's per-mount trash *browse* pass -- a filesystem with no
/// trash on it yet must come back with nothing found, not a freshly
/// created empty trash directory.
fn find_method_one_root(topdir: &Path, uid: u32) -> Option<PathBuf> {
    let dot_trash = method_one_dot_trash_dir(topdir)?;
    let user_dir = dot_trash.join(uid.to_string());
    if !find_existing_safe_trash_root(&user_dir, uid) {
        return None;
    }
    Some(user_dir)
}

/// The read-only counterpart to [`try_method_two`] -- see
/// [`find_method_one_root`]'s own doc comment for why this never creates
/// `$topdir/.Trash-$uid`.
fn find_method_two_root(topdir: &Path, uid: u32) -> Option<PathBuf> {
    let user_dir = topdir.join(format!(".Trash-{uid}"));
    if !find_existing_safe_trash_root(&user_dir, uid) {
        return None;
    }
    Some(user_dir)
}

/// Creates `root`, `root/files`, and `root/info` (each idempotently, at
/// exactly mode `0700` regardless of umask -- `mkdirat`'s own mode
/// parameter is umask-modulated, so an explicit `chmodat` follows every
/// creation to guarantee the spec's privacy requirement rather than
/// hoping the caller's umask happens to cooperate).
fn create_trash_root(root: &Path) -> Result<(), TrashError> {
    ensure_dir_mode_0700(root)?;
    ensure_dir_mode_0700(&root.join("files"))?;
    ensure_dir_mode_0700(&root.join("info"))?;
    Ok(())
}

fn ensure_dir_mode_0700(path: &Path) -> Result<(), TrashError> {
    let mode = Mode::from_raw_mode(0o700);
    match fs::mkdirat(CWD, path, mode) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(e) => return Err(io_err_at(path, e)),
    }
    fs::chmodat(CWD, path, mode, AtFlags::empty()).map_err(|e| io_err_at(path, e))
}

/// The bound on how many `name (N)` candidates [`unique_trash_name`] will
/// try before giving up -- same generous, "something is genuinely wrong if
/// we hit this" bound `duet_ops::executor::auto_rename_target` uses for
/// the identical shape of search.
const UNIQUE_NAME_MAX_ATTEMPTS: u32 = 1000;

/// Finds the first name (`original_name` itself, or `stem (2).ext`, `stem
/// (3).ext`, ...) with no existing entry in either `files_dir` or
/// `info_dir` -- the plan-time equivalent of `executor::auto_rename_target`
/// (see the module doc comment's "Why plan-time name resolution" section
/// for why this runs here, at planning time, instead of live in the
/// executor).
fn unique_trash_name(
    files_dir: &Path,
    info_dir: &Path,
    original_name: &str,
    reservations: &TrashReservations,
) -> io::Result<String> {
    if !trash_name_taken(files_dir, info_dir, original_name, reservations) {
        return Ok(original_name.to_string());
    }
    let (stem, ext) = split_stem_ext(original_name);
    for n in 2..=UNIQUE_NAME_MAX_ATTEMPTS {
        let candidate = match &ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        if !trash_name_taken(files_dir, info_dir, &candidate, reservations) {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "could not find a free trash name near {original_name:?} after \
             {UNIQUE_NAME_MAX_ATTEMPTS} attempts"
        ),
    ))
}

fn trash_name_taken(
    files_dir: &Path,
    info_dir: &Path,
    name: &str,
    reservations: &TrashReservations,
) -> bool {
    let content = files_dir.join(name);
    let info = info_dir.join(format!("{name}.trashinfo"));
    reservations.0.contains(&content)
        || fs::statat(CWD, &content, AtFlags::SYMLINK_NOFOLLOW).is_ok()
        || fs::statat(CWD, &info, AtFlags::SYMLINK_NOFOLLOW).is_ok()
}

fn split_stem_ext(name: &str) -> (String, Option<String>) {
    let p = Path::new(name);
    let stem = p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_string();
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_string());
    (stem, ext)
}

/// Standard URI percent-encoding (RFC 3986 unreserved bytes, plus `/` left
/// literal as the path separator, left untouched; everything else --
/// including a space, which becomes `%20`, never `+` -- escaped as
/// `%XX`). Operates byte-wise on `s`'s UTF-8 representation, so a
/// multi-byte UTF-8 sequence for a non-ASCII character is correctly
/// escaped one byte at a time, exactly as RFC 3986 requires.
fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

fn format_trashinfo(path_field: &str, civil: (i32, u32, u32, u32, u32, u32)) -> String {
    let (y, mo, d, hh, mm, ss) = civil;
    format!(
        "[Trash Info]\nPath={path_field}\nDeletionDate={y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}\n"
    )
}

fn unix_secs(t: SystemTime) -> i64 {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// Mirrors glibc/musl's `struct tm` -- see the module doc comment's "Local
/// time without a date/time crate dependency" section.
#[repr(C)]
struct CTm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    tm_gmtoff: i64,
    tm_zone: *const c_char,
}

unsafe extern "C" {
    fn localtime_r(timep: *const i64, result: *mut CTm) -> *mut CTm;
    /// [`local_civil_time_to_unix`]'s own backend -- the standard, POSIX-
    /// specified inverse of `localtime_r`: takes a `struct tm` (local civil
    /// time, per `TZ`/`/etc/localtime` — the same zone database
    /// `localtime_r` itself already reads), fills in `tm_wday`/`tm_yday`,
    /// normalises out-of-range fields, and returns the corresponding
    /// `time_t`. `tm_isdst = -1` (set by every caller here) tells `mktime`
    /// to determine DST itself rather than trust a caller-supplied guess —
    /// exactly right for a `.trashinfo` `DeletionDate=`, which carries no
    /// DST flag of its own to round-trip.
    fn mktime(tm: *mut CTm) -> i64;
}

/// `unix_secs` broken down into the local timezone's civil time (per
/// `TZ`/`/etc/localtime`) as `(year, month, day, hour, minute, second)` --
/// `month`/`day` are 1-based, matching `.trashinfo`'s own `Y-M-DTh:m:s`
/// format directly.
fn local_civil_time(unix_secs_value: i64) -> (i32, u32, u32, u32, u32, u32) {
    let mut tm: CTm = unsafe { std::mem::zeroed() };
    // SAFETY: `localtime_r` (the reentrant variant -- unlike plain
    // `localtime`, it never returns a pointer into thread-local/static
    // storage this call doesn't own) writes into `tm`, a fully-owned,
    // correctly-sized local, and both pointers are valid for the duration
    // of this one call.
    let ok = unsafe { !localtime_r(&unix_secs_value, &mut tm).is_null() };
    if !ok {
        // Should not happen on Linux -- `localtime_r` only fails if the
        // year over/underflows `struct tm`'s `int` fields, far outside any
        // real deletion date. Degrade to UTC rather than panicking.
        return utc_civil_time(unix_secs_value);
    }
    (
        tm.tm_year + 1900,
        (tm.tm_mon + 1) as u32,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
        tm.tm_sec as u32,
    )
}

/// UTC civil-time fallback for [`local_civil_time`]'s never-expected-in-
/// practice error path -- Howard Hinnant's `civil_from_days` algorithm,
/// the same one `duet-ui::file_table::civil_from_unix` uses (reimplemented
/// independently here rather than shared: this crate doesn't depend on
/// `duet-ui`, and it's a few lines of well-known public-domain algebra,
/// matching this codebase's existing "small load-bearing primitives are
/// duplicated per-crate rather than pulling in a cross-crate dependency
/// for them" precedent -- see `duet_ops::executor::partial_file_name`'s
/// own doc comment for another instance of the same convention).
fn utc_civil_time(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let hour = (time_of_day / 3600) as u32;
    let minute = ((time_of_day % 3600) / 60) as u32;
    let second = (time_of_day % 60) as u32;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };

    (y as i32, m, d, hour, minute, second)
}

/// The exact inverse of [`local_civil_time`], via `mktime(3)` -- see the
/// `unsafe extern "C"` block's own doc comment on `tm_isdst = -1`. Returns
/// `None` only in the practically-unreachable case `mktime` itself signals
/// failure (`(time_t) -1`; per POSIX this also happens to be a legitimate
/// return value for one second in 1969, a date no real `.trashinfo` will
/// ever carry, so this function does not try to disambiguate that case
/// specially).
fn local_civil_time_to_unix(y: i32, mo: u32, d: u32, hh: u32, mm: u32, ss: u32) -> Option<i64> {
    let mut tm: CTm = unsafe { std::mem::zeroed() };
    tm.tm_sec = ss as i32;
    tm.tm_min = mm as i32;
    tm.tm_hour = hh as i32;
    tm.tm_mday = d as i32;
    tm.tm_mon = mo as i32 - 1;
    tm.tm_year = y - 1900;
    tm.tm_isdst = -1;
    // SAFETY: `tm` is a fully-owned, correctly-sized local; `mktime` only
    // reads/writes through the one valid pointer we hand it, for the
    // duration of this one call.
    let secs = unsafe { mktime(&mut tm) };
    if secs == -1 { None } else { Some(secs) }
}

fn system_time_from_unix(secs: i64) -> SystemTime {
    if secs >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_secs((-secs) as u64)
    }
}

/// One entry [`list_trash_entries`] found -- a `.trashinfo` sidecar plus
/// its paired content, with `Path=`/`DeletionDate=` already parsed back
/// into structured data. `duet_ops::trash_restore` is the intended
/// consumer: `content_path`/`info_path` are exactly what a restore or
/// permanent-purge `Step::Rename`/`Step::Remove` needs, and `original_path`
/// is where a restore puts the content back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashEntry {
    /// `$trash/files/<name>` -- where the trashed content itself currently
    /// lives.
    pub content_path: PathBuf,
    /// `$trash/info/<name>.trashinfo` -- the sidecar this entry was parsed
    /// from.
    pub info_path: PathBuf,
    /// The path this entry should be restored to -- `.trashinfo`'s own
    /// `Path=` field, percent-decoded and, for a per-mount trash entry,
    /// resolved against that root's own topdir (see the module doc
    /// comment's "T-5.3.2 phase 1" section).
    pub original_path: PathBuf,
    /// `.trashinfo`'s own `DeletionDate=` field, parsed back from local
    /// civil time via [`local_civil_time_to_unix`] -- the exact inverse of
    /// what [`resolve_trash_destination_at`] wrote via [`local_civil_time`].
    pub deleted_at: SystemTime,
}

/// Enumerates every trash entry `duet` (or any other freedesktop-spec tool)
/// has left under `xdg_data_home`'s own home trash (`$xdg_data_home/Trash`)
/// *and* under every other mounted filesystem's own per-mount trash --
/// see the module doc comment's "T-5.3.2 phase 2" section for the full
/// design, and its "tolerate garbage, don't propagate it" per-entry
/// parse-failure convention (unchanged from phase 1).
///
/// An `xdg_data_home` with no `Trash` directory at all (nothing has ever
/// been trashed there) is not an error -- it produces an empty `Vec` for
/// the home-trash half, mirroring [`resolve_trash_destination`]'s own
/// "create on demand" stance on the same directory from the write side. A
/// failure reading or parsing `/proc/self/mountinfo` (shouldn't happen on
/// a real Linux system, but handled honestly) is likewise not an error --
/// it degrades to home-trash-only, since home trash is real, useful data
/// on its own even when the per-mount discovery pass can't run at all.
///
/// # Errors
/// [`TrashError::Io`] if `$xdg_data_home/Trash/info` exists but can't be
/// read at all (a genuine, surfaced failure -- a real permission problem on
/// a directory this module's own writer always creates at mode `0700`
/// owned by the current user, so a read failure here means something
/// outside `duet`'s own control changed it). A per-mount trash root that
/// exists but can't be read is *not* surfaced this way -- see
/// [`list_entries_from_candidate_topdirs`]'s own doc comment.
pub fn list_trash_entries(xdg_data_home: &Path) -> Result<Vec<TrashEntry>, TrashError> {
    list_trash_entries_with_mounts(xdg_data_home, &MountScan::System)
}

/// Where [`list_trash_entries_with_mounts`] looks for *per-mount* trash
/// roots, on top of the home trash it always scans.
///
/// Exists because the per-mount pass is the one part of trash listing that
/// reads global machine state (`/proc/self/mountinfo`): a caller that has
/// redirected `$XDG_DATA_HOME` into a scratch directory is still, under
/// [`MountScan::System`], going to see every real mounted filesystem's own
/// trash -- exactly what made `duet-ui`'s trash-browser tests fail on any
/// developer machine with a real trashed file on a second drive. Tests
/// (and any future sandboxed/embedded use) pass [`MountScan::Explicit`]
/// instead, which touches nothing outside the paths named.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MountScan {
    /// Discover candidates from the real `/proc/self/mountinfo` -- the
    /// production behaviour, and [`list_trash_entries`]'s.
    #[default]
    System,
    /// Probe exactly these mount points (the same safety checks and the
    /// same "never create anything" rule apply) and nothing else. An empty
    /// list means home trash only.
    Explicit(Vec<PathBuf>),
}

/// [`list_trash_entries`] with the per-mount candidate source chosen by the
/// caller -- see [`MountScan`]. [`MountScan::Explicit`] candidates go
/// through the same [`dedup_and_exclude_home_dev`] filter as the real
/// mount table does, so the two variants differ only in where the list
/// comes from, never in what is done with it.
///
/// # Errors
/// Same as [`list_trash_entries`].
pub fn list_trash_entries_with_mounts(
    xdg_data_home: &Path,
    mounts: &MountScan,
) -> Result<Vec<TrashEntry>, TrashError> {
    let mut entries = list_trash_root(&xdg_data_home.join("Trash"), None)?;
    let raw_candidates = match mounts {
        MountScan::System => real_mountinfo_candidates(),
        MountScan::Explicit(paths) => paths.clone(),
    };
    let candidates = dedup_and_exclude_home_dev(xdg_data_home, &raw_candidates);
    entries.extend(list_entries_from_candidate_topdirs(&candidates));
    Ok(entries)
}

/// [`list_trash_entries`] with an explicit candidate topdir list instead of
/// reading real `/proc/self/mountinfo` -- exists purely so tests can prove
/// the home-trash-scan-plus-per-mount-merge wiring works without needing a
/// real second mounted filesystem, the same shape of seam
/// [`resolve_trash_destination_at`] already provides for injecting
/// `SystemTime::now()`.
///
/// Deliberately does *not* re-run [`dedup_and_exclude_home_dev`] on
/// `candidates` -- that dev-comparison filter is its own, independently
/// and deterministically tested unit (real `TempDir`s under the same `/tmp`
/// share one `st_dev` in most sandboxes, which would make a filter re-run
/// here nondeterministically swallow a test's deliberately-distinct
/// "per-mount" `TempDir`); callers of this test-only seam are expected to
/// pass an already-appropriate candidate list, exactly as
/// [`list_entries_from_candidate_topdirs`] itself (which this delegates
/// to) is documented not to know about the exclusion rule at all.
#[cfg(test)]
fn list_trash_entries_with_candidates(
    xdg_data_home: &Path,
    candidates: &[PathBuf],
) -> Result<Vec<TrashEntry>, TrashError> {
    let mut entries = list_trash_root(&xdg_data_home.join("Trash"), None)?;
    entries.extend(list_entries_from_candidate_topdirs(candidates));
    Ok(entries)
}

/// Given a list of candidate topdir paths, finds which ones already have a
/// usable, *already-existing* trash root (via [`find_method_one_root`]/
/// [`find_method_two_root`] -- the read-only, never-creates-anything
/// counterparts to [`try_method_one`]/[`try_method_two`]) and returns every
/// entry found in each, merged into one `Vec`. A candidate with no usable
/// trash root at all -- nothing has ever been trashed there, or the safety
/// checks fail -- contributes nothing and is not an error: a read-only
/// browse pass over a filesystem that happens to have no trash on it yet
/// must find nothing, not create an empty trash directory as a side effect
/// of merely looking (unlike [`resolve_trash_location`]'s write-side
/// on-demand creation).
///
/// Pure and injectable: does not itself read or parse
/// `/proc/self/mountinfo` (see [`real_mountinfo_candidates`], the one real
/// caller building the list this is fed in production) and does not know
/// about [`dedup_and_exclude_home_dev`]'s exclusion rule at all -- only
/// [`list_trash_entries`]'s own orchestration does, matching
/// `topdir_walk_logic_stops_exactly_where_st_dev_changes`'s own precedent
/// of testing a walk/probe against an injectable input rather than a real
/// mount table.
///
/// A per-mount trash root that exists (passes the safety checks above) but
/// whose `info` directory can't actually be read for some other reason is
/// tolerated the same way a single malformed `.trashinfo` is: that one
/// root contributes nothing, the rest of the candidates are unaffected.
fn list_entries_from_candidate_topdirs(candidates: &[PathBuf]) -> Vec<TrashEntry> {
    let uid = rustix::process::getuid().as_raw();
    let mut entries = Vec::new();
    for topdir in candidates {
        let root = find_method_one_root(topdir, uid).or_else(|| find_method_two_root(topdir, uid));
        let Some(root) = root else { continue };
        if let Ok(found) = list_trash_root(&root, Some(topdir)) {
            entries.extend(found);
        }
    }
    entries
}

/// [`list_trash_entries`]'s dev-based hygiene pass over a raw candidate
/// topdir list: drops whichever candidate's device matches
/// `xdg_data_home`'s own (already covered by the home-trash scan one layer
/// up -- scanning it again as a "per-mount" trash would double-count every
/// home-trash entry), and deduplicates the rest by [`dev_of`] (a bind mount
/// reachable via two different mount-point paths must not be scanned
/// twice). A candidate that can't even be `stat`'d (gone by the time this
/// runs, e.g.) is silently dropped -- the same "tolerate garbage" stance as
/// everything else in this module.
fn dedup_and_exclude_home_dev(xdg_data_home: &Path, candidates: &[PathBuf]) -> Vec<PathBuf> {
    let home_dev = dev_of(xdg_data_home).ok();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for candidate in candidates {
        let Ok(dev) = dev_of(candidate) else { continue };
        if Some(dev) == home_dev {
            continue;
        }
        if !seen.insert(dev) {
            continue;
        }
        out.push(candidate.clone());
    }
    out
}

/// Filesystem types that can never meaningfully hold a per-mount trash and
/// would just be wasted probing -- pseudo/virtual filesystems
/// (`proc`, `sysfs`, `cgroup`, ...) that have no real on-disk `.Trash{,-
/// $uid}` concept at all, plus `tmpfs`/`overlay`/`squashfs` specifically to
/// avoid probing inside every snap/flatpak package mount, which can number
/// in the dozens on a real desktop and would never have user-trashed
/// content. Not an exhaustive enumeration -- see [`is_probeable_fstype`].
const NON_PROBEABLE_FSTYPES: &[&str] = &[
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "devpts",
    "devtmpfs",
    "securityfs",
    "debugfs",
    "tracefs",
    "pstore",
    "bpf",
    "mqueue",
    "hugetlbfs",
    "autofs",
    "binfmt_misc",
    "tmpfs",
    "overlay",
    "squashfs",
    // Not real on-disk filesystems either: a network-namespace handle
    // (containers/`docker network` create one per namespace) and a
    // pseudo-fs exposing kernel config -- neither can hold a trash.
    "nsfs",
    "configfs",
    "fusectl",
];

fn is_probeable_fstype(fstype: &str) -> bool {
    !NON_PROBEABLE_FSTYPES.contains(&fstype)
}

/// [`list_trash_entries`]'s real, production candidate source: reads
/// `/proc/self/mountinfo`, parses it via [`parse_mountinfo`], and returns
/// every surviving mount point after [`is_probeable_fstype`] filters out
/// pseudo-filesystems. A read failure (shouldn't happen on a real Linux
/// system) degrades to an empty list rather than propagating an error --
/// see the module doc comment's "T-5.3.2 phase 2" section.
fn real_mountinfo_candidates() -> Vec<PathBuf> {
    let Ok(content) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    parse_mountinfo(&content)
        .into_iter()
        .filter(|line| is_probeable_fstype(&line.fstype))
        .map(|line| line.mount_point)
        .collect()
}

/// One `/proc/self/mountinfo` line's two facts this module actually needs
/// -- see [`parse_mountinfo`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct MountInfoLine {
    mount_point: PathBuf,
    fstype: String,
}

/// Parses `/proc/self/mountinfo`'s own text content (`man 5
/// proc_pid_mountinfo`) into one [`MountInfoLine`] per well-formed line,
/// silently skipping any line that doesn't parse -- the same "tolerate
/// garbage, don't propagate it" convention this module already applies to
/// a malformed `.trashinfo` (see [`parse_trash_info_file`]'s own doc
/// comment). Takes the file's already-read text content, rather than
/// reading `/proc/self/mountinfo` itself, so it can be unit-tested against
/// literal fixture strings without needing a real `/proc` at all (see
/// [`real_mountinfo_candidates`] for the one real caller that does the
/// actual file read).
///
/// Each line's fixed-position fields (mount ID, parent ID, `major:minor`,
/// root, mount point, mount options) sit *before* a literal `" - "` token;
/// the fields between the mount options and that separator are a
/// variable-length list of optional fields (a `master:N`/`shared:N`/etc.
/// peer-group tag) -- exactly why the separator exists at all, so this
/// parser does not assume any particular count of them: it splits on the
/// separator first, and only then reads fixed-position fields out of each
/// side (mount point is always index 4, 0-indexed, in the part before the
/// separator; filesystem type is the first field after it).
fn parse_mountinfo(content: &str) -> Vec<MountInfoLine> {
    let mut out = Vec::new();
    for line in content.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            continue; // malformed -- no separator at all
        };
        let before_fields: Vec<&str> = before.split(' ').collect();
        let Some(raw_mount_point) = before_fields.get(4) else {
            continue;
        };
        let Some(mount_point) = unescape_octal(raw_mount_point) else {
            continue;
        };
        let Some(fstype) = after.split(' ').next().filter(|s| !s.is_empty()) else {
            continue;
        };
        out.push(MountInfoLine {
            mount_point: PathBuf::from(mount_point),
            fstype: fstype.to_string(),
        });
    }
    out
}

/// Undoes `/proc/self/mountinfo`'s octal escaping of whitespace and
/// backslash in a path field (` ` -> `\040`, tab -> `\011`, newline ->
/// `\012`, `\\` -> `\134`) -- a real-world mount point (removable media
/// with a space in its volume label, e.g.) actually contains these, so a
/// literal space-split at the field level (already done by
/// [`parse_mountinfo`]) is not enough on its own; each field's own content
/// must also be unescaped. `None` for a trailing/truncated `\` escape or a
/// `\NNN` sequence that isn't valid octal -- [`parse_mountinfo`] skips the
/// whole line in that case, the same "tolerate garbage" stance as
/// everywhere else in this module.
fn unescape_octal(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let digits = bytes.get(i + 1..i + 4)?;
            let digits = std::str::from_utf8(digits).ok()?;
            out.push(u8::from_str_radix(digits, 8).ok()?);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// [`list_trash_entries`]'s per-root worker: reads every `*.trashinfo` file
/// directly under `root/info`, parses it, and pairs it with its content
/// under `root/files`. `topdir` is `None` for home trash (`Path=` is
/// already absolute) or `Some(topdir)` for a per-mount trash root
/// (`Path=` is relative to `topdir` -- see [`resolve_trash_destination_at`]'s
/// own `path_field` construction, which this is the exact inverse of).
///
/// Not exposed publicly: [`list_trash_entries`] itself calls this once with
/// `topdir: None` for home trash, and again (via
/// [`list_entries_from_candidate_topdirs`]) with `topdir: Some(_)` once per
/// discovered per-mount trash root (see the module doc comment's "T-5.3.2
/// phase 2" section) -- both branches are real, load-bearing code paths.
fn list_trash_root(root: &Path, topdir: Option<&Path>) -> Result<Vec<TrashEntry>, TrashError> {
    let info_dir = root.join("info");
    let files_dir = root.join("files");
    let read_dir = match std::fs::read_dir(&info_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(TrashError::Io {
                path: info_dir,
                source: e,
            });
        }
    };

    let mut entries = Vec::new();
    for dir_entry in read_dir {
        // A single unreadable directory entry (rare -- a concurrent
        // deletion mid-scan, e.g.) is skipped, not fatal to the whole
        // listing -- same "tolerate garbage" stance as a malformed
        // `.trashinfo` file below.
        let Ok(dir_entry) = dir_entry else { continue };
        let info_path = dir_entry.path();
        if info_path.extension().and_then(|e| e.to_str()) != Some("trashinfo") {
            continue;
        }
        let Some(name) = info_path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(parsed) = parse_trash_info_file(&info_path) else {
            continue;
        };
        let original_path = match topdir {
            None => PathBuf::from(&parsed.decoded_path),
            Some(topdir) => topdir.join(&parsed.decoded_path),
        };
        entries.push(TrashEntry {
            content_path: files_dir.join(name),
            info_path,
            original_path,
            deleted_at: parsed.deleted_at,
        });
    }
    Ok(entries)
}

/// A `.trashinfo` file's two fields, already decoded/parsed -- see
/// [`parse_trash_info_file`].
struct ParsedTrashInfo {
    decoded_path: String,
    deleted_at: SystemTime,
}

/// Reads and parses one `.trashinfo` file, returning `None` for absolutely
/// any way it can fail to be a well-formed, understandable entry (missing
/// file, not UTF-8, no `[Trash Info]` header, missing/unparseable `Path=`
/// or `DeletionDate=`, an out-of-range date `mktime` itself rejects) --
/// [`list_trash_root`]'s caller treats every one of these identically:
/// skip this entry, keep scanning. Deliberately not a `Result`: there is
/// exactly one caller, and it never needs to distinguish *why* a
/// `.trashinfo` didn't parse, only *whether* it did.
fn parse_trash_info_file(info_path: &Path) -> Option<ParsedTrashInfo> {
    let content = std::fs::read_to_string(info_path).ok()?;
    let (path_field, date_field) = parse_trashinfo_fields(&content)?;
    let decoded_path = percent_decode_path(&path_field)?;
    let (y, mo, d, hh, mm, ss) = parse_deletion_date(&date_field)?;
    let secs = local_civil_time_to_unix(y, mo, d, hh, mm, ss)?;
    Some(ParsedTrashInfo {
        decoded_path,
        deleted_at: system_time_from_unix(secs),
    })
}

/// A small, honest `[Trash Info]`/`Path=`/`DeletionDate=` line parser --
/// not a general INI parser (this format has exactly one section and two
/// keys; a real config-parsing dependency would be solving a much bigger
/// problem than this module has), and not tolerant of anything the spec
/// doesn't actually require: a `.trashinfo` written by this module's own
/// [`format_trashinfo`] always starts with the `[Trash Info]` header line,
/// so a file missing it is either not a trash sidecar at all or corrupted
/// enough not to trust -- [`parse_trash_info_file`] treats both the same
/// way (skip).
///
/// Lines are matched by exact key prefix (`Path=`/`DeletionDate=`); a
/// `Path=` value containing `=` (legal -- `=` isn't percent-encoded by
/// [`percent_encode_path`] since it's an RFC 3986 unreserved... actually it
/// is escaped, being outside the allowed set -- so this never actually
/// arises for a sidecar this module wrote, but the prefix-match approach
/// handles it correctly regardless, taking everything after the first `=`
/// as the value) is read whole, not truncated at a later `=`.
fn parse_trashinfo_fields(content: &str) -> Option<(String, String)> {
    let mut saw_header = false;
    let mut path_field = None;
    let mut date_field = None;
    for line in content.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line == "[Trash Info]" {
            saw_header = true;
        } else if let Some(rest) = line.strip_prefix("Path=") {
            path_field = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("DeletionDate=") {
            date_field = Some(rest.to_string());
        }
    }
    if !saw_header {
        return None;
    }
    Some((path_field?, date_field?))
}

/// The exact inverse of [`percent_encode_path`]: standard `%XX` percent-
/// decoding, byte-wise, with the decoded bytes re-assembled as UTF-8 at the
/// end (matching the encoder's own byte-wise treatment of a UTF-8 source
/// string). `None` for a truncated `%` escape, an invalid hex pair, or a
/// decoded byte sequence that isn't valid UTF-8 -- any of which means this
/// wasn't a `Path=` value this module (or any correctly-percent-encoding
/// tool) actually wrote.
fn percent_decode_path(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Parses `.trashinfo`'s `DeletionDate=` value, `Y-M-DTh:m:s` (ISO 8601,
/// no timezone suffix -- the exact shape [`format_trashinfo`] writes), into
/// `(year, month, day, hour, minute, second)` -- the same tuple shape
/// [`local_civil_time`] produces, so it feeds
/// [`local_civil_time_to_unix`] directly. `None` for anything that isn't
/// exactly this shape (extra/missing components, non-numeric fields) --
/// this module makes no attempt to also accept other ISO 8601 variants
/// (fractional seconds, a `Z`/offset suffix) that `.trashinfo` never
/// carries and this module's own writer never produces.
fn parse_deletion_date(s: &str) -> Option<(i32, u32, u32, u32, u32, u32)> {
    let (date, time) = s.split_once('T')?;
    let mut date_parts = date.split('-');
    let y: i32 = date_parts.next()?.parse().ok()?;
    let mo: u32 = date_parts.next()?.parse().ok()?;
    let d: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() {
        return None;
    }
    let mut time_parts = time.split(':');
    let hh: u32 = time_parts.next()?.parse().ok()?;
    let mm: u32 = time_parts.next()?.parse().ok()?;
    let ss: u32 = time_parts.next()?.parse().ok()?;
    if time_parts.next().is_some() {
        return None;
    }
    Some((y, mo, d, hh, mm, ss))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;

    // -- percent-encoding ----------------------------------------------

    #[test]
    fn percent_encode_leaves_unreserved_bytes_and_slash_untouched() {
        assert_eq!(
            percent_encode_path("/home/u/docs/report-final_v2.txt"),
            "/home/u/docs/report-final_v2.txt"
        );
    }

    #[test]
    fn percent_encode_escapes_spaces_as_percent_20_not_plus() {
        let encoded = percent_encode_path("/home/u/my file (copy).txt");
        assert!(encoded.contains("%20"), "{encoded}");
        assert!(!encoded.contains('+'), "{encoded}");
        assert!(encoded.contains("%28"), "{encoded}"); // '('
        assert!(encoded.contains("%29"), "{encoded}"); // ')'
    }

    #[test]
    fn percent_encode_round_trips_reserved_and_non_ascii_bytes() {
        let original = "/home/u/déjà vu?.txt";
        let encoded = percent_encode_path(original);
        // Decode it back by hand (this module has no decoder of its own --
        // GNOME/KDE's own trash readers are the real consumer -- so this
        // test proves the encoding is reversible via the standard
        // percent-decoding algorithm, not via a decoder this module
        // happens to also implement).
        let mut decoded = Vec::new();
        let bytes = encoded.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                decoded.push(u8::from_str_radix(hex, 16).unwrap());
                i += 3;
            } else {
                decoded.push(bytes[i]);
                i += 1;
            }
        }
        assert_eq!(String::from_utf8(decoded).unwrap(), original);
    }

    // -- format_trashinfo -------------------------------------------------

    #[test]
    fn format_trashinfo_matches_the_exact_spec_shape() {
        let content = format_trashinfo("/home/u/deleted.txt", (2026, 8, 30, 14, 22, 7));
        assert_eq!(
            content,
            "[Trash Info]\nPath=/home/u/deleted.txt\nDeletionDate=2026-08-30T14:22:07\n"
        );
    }

    #[test]
    fn format_trashinfo_pads_single_digit_components() {
        let content = format_trashinfo("rel/path.txt", (2026, 1, 2, 3, 4, 5));
        assert!(
            content.contains("DeletionDate=2026-01-02T03:04:05"),
            "{content}"
        );
    }

    // -- utc_civil_time / local_civil_time ---------------------------------

    #[test]
    fn utc_civil_time_matches_a_known_instant() {
        // 2026-08-30T00:00:00Z, computed independently via `date -u -d
        // 2026-08-30T00:00:00Z +%s` at the time this test was written.
        let (y, mo, d, hh, mm, ss) = utc_civil_time(1_788_048_000);
        assert_eq!((y, mo, d, hh, mm, ss), (2026, 8, 30, 0, 0, 0));
    }

    #[test]
    fn utc_civil_time_round_trips_epoch() {
        assert_eq!(utc_civil_time(0), (1970, 1, 1, 0, 0, 0));
    }

    /// Cross-checks [`local_civil_time`] against the real `date` binary,
    /// independently of this module's own algorithm -- not verified if
    /// `date` isn't available, per this codebase's "don't claim untested
    /// results" convention (`duet-vfs::local::probe`'s own tests already
    /// establish this precedent for environment-dependent checks).
    #[test]
    fn local_civil_time_matches_the_real_date_command() {
        let secs = 1_788_048_000i64; // 2026-08-30T00:00:00Z
        let output = std::process::Command::new("date")
            .arg("-d")
            .arg(format!("@{secs}"))
            .arg("+%Y-%m-%d %H:%M:%S")
            .output();
        let Ok(output) = output else {
            eprintln!(
                "local_civil_time_matches_the_real_date_command: `date` not available -- \
                 not verified here"
            );
            return;
        };
        if !output.status.success() {
            eprintln!(
                "local_civil_time_matches_the_real_date_command: `date` exited non-zero -- \
                 not verified here"
            );
            return;
        }
        let expected = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let (y, mo, d, hh, mm, ss) = local_civil_time(secs);
        let ours = format!("{y:04}-{mo:02}-{d:02} {hh:02}:{mm:02}:{ss:02}");
        assert_eq!(ours, expected);
    }

    // -- topdir_of ----------------------------------------------------------

    #[test]
    fn topdir_of_a_path_entirely_within_one_tmpfs_tempdir_is_the_tempdir_itself() {
        // Every path here shares one `st_dev` (the whole TempDir is one
        // tmpfs mount in this test environment) up to `/tmp` itself, which
        // is where the walk would naturally stop climbing -- so rather
        // than assert a specific directory (fragile, environment-
        // dependent), assert the *property* topdir_of exists to prove:
        // walking from a nested path never crosses back below where it
        // started, and never panics on a real path.
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        let file = nested.join("file.txt");
        std::fs::write(&file, b"x").unwrap();

        let topdir = topdir_of(&file).unwrap();
        assert!(
            file.starts_with(&topdir),
            "topdir {topdir:?} must be an ancestor of {file:?}"
        );
        // The nested tempdir itself is not a distinct mount, so climbing
        // must go at least as far up as its own parent.
        assert!(
            topdir == Path::new("/") || dir.path().starts_with(&topdir) || topdir == dir.path(),
            "topdir {topdir:?} unexpectedly sits *below* the tempdir {:?}",
            dir.path()
        );
    }

    /// The core `st_dev`-comparison logic in isolation, injected via a
    /// synthetic device map rather than a real second mounted filesystem
    /// (bind-mounts/loop devices need root or a container this test
    /// environment cannot assume -- see this function's own doc comment
    /// for why a real cross-filesystem integration test isn't included:
    /// `duet-vfs`'s own cross-device tests, e.g.
    /// `probe::tests::probes_real_btrfs_if_available`, hit the identical
    /// constraint and handle it the same way, by dynamically discovering
    /// a real second mount and skipping -- not failing -- when none is
    /// available. That approach doesn't fit *this* specific check, though:
    /// it needs a directory *boundary* at a controlled location, not just
    /// "any filesystem of a given kind somewhere on the machine," which a
    /// dynamically-discovered mount can't guarantee).
    #[test]
    fn topdir_walk_logic_stops_exactly_where_st_dev_changes() {
        // A hand-written stand-in for the real walk, using an injectable
        // "device of this path" function instead of a real `stat` --
        // proves the walk-and-compare algorithm itself (stop climbing the
        // instant the parent's device differs) independently of whether a
        // real second filesystem is mounted anywhere on this machine.
        fn topdir_of_with(path: &Path, dev_of: impl Fn(&Path) -> u64) -> PathBuf {
            let target_dev = dev_of(path);
            let mut topdir = path.parent().unwrap().to_path_buf();
            loop {
                if topdir == Path::new("/") {
                    return topdir;
                }
                let Some(parent) = topdir.parent() else {
                    return topdir;
                };
                if dev_of(parent) != target_dev {
                    return topdir;
                }
                topdir = parent.to_path_buf();
            }
        }

        // Simulated layout: "/mnt/other" is a distinct mount (dev 2) from
        // its own parent "/mnt" (dev 1, same as everything above it).
        let dev_of = |p: &Path| -> u64 {
            if p == Path::new("/mnt/other") || p.starts_with("/mnt/other/") {
                2
            } else {
                1
            }
        };

        let topdir = topdir_of_with(Path::new("/mnt/other/a/b/file.txt"), dev_of);
        assert_eq!(topdir, PathBuf::from("/mnt/other"));

        let topdir_root = topdir_of_with(Path::new("/mnt/elsewhere/file.txt"), dev_of);
        assert_eq!(
            topdir_root,
            PathBuf::from("/"),
            "everything outside /mnt/other shares dev 1 all the way to /"
        );
    }

    // -- method 1 vs method 2 fallback --------------------------------------

    #[test]
    fn method_one_is_used_when_dot_trash_has_the_sticky_bit_set() {
        let topdir = TempDir::new().unwrap();
        let dot_trash = topdir.path().join(".Trash");
        std::fs::create_dir(&dot_trash).unwrap();
        std::fs::set_permissions(&dot_trash, std::fs::Permissions::from_mode(0o1777)).unwrap();

        let uid = rustix::process::getuid().as_raw();
        let root = try_method_one(topdir.path(), uid).expect("method 1 should be usable");
        assert_eq!(root, dot_trash.join(uid.to_string()));
        assert!(root.join("files").is_dir());
        assert!(root.join("info").is_dir());
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn method_one_falls_through_when_dot_trash_is_missing() {
        let topdir = TempDir::new().unwrap();
        let uid = rustix::process::getuid().as_raw();
        assert!(try_method_one(topdir.path(), uid).is_none());
    }

    #[test]
    fn method_one_falls_through_when_dot_trash_lacks_the_sticky_bit() {
        let topdir = TempDir::new().unwrap();
        let dot_trash = topdir.path().join(".Trash");
        std::fs::create_dir(&dot_trash).unwrap();
        std::fs::set_permissions(&dot_trash, std::fs::Permissions::from_mode(0o777)).unwrap();

        let uid = rustix::process::getuid().as_raw();
        assert!(try_method_one(topdir.path(), uid).is_none());
    }

    #[test]
    fn method_one_falls_through_when_dot_trash_is_a_symlink() {
        let topdir = TempDir::new().unwrap();
        let real_dir = topdir.path().join("real-trash");
        std::fs::create_dir(&real_dir).unwrap();
        std::fs::set_permissions(&real_dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let dot_trash = topdir.path().join(".Trash");
        std::os::unix::fs::symlink(&real_dir, &dot_trash).unwrap();

        let uid = rustix::process::getuid().as_raw();
        assert!(
            try_method_one(topdir.path(), uid).is_none(),
            "a symlinked .Trash is a known spec attack vector and must never be trusted"
        );
    }

    #[test]
    fn method_two_creates_dot_trash_dash_uid_at_mode_0700() {
        let topdir = TempDir::new().unwrap();
        let uid = rustix::process::getuid().as_raw();
        let root = try_method_two(topdir.path(), uid).expect("method 2 must always be usable");
        assert_eq!(root, topdir.path().join(format!(".Trash-{uid}")));
        assert!(root.join("files").is_dir());
        assert!(root.join("info").is_dir());
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn neither_method_usable_on_a_read_only_topdir_is_a_clear_per_target_failure() {
        let topdir = TempDir::new().unwrap();
        std::fs::set_permissions(topdir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

        let uid = rustix::process::getuid().as_raw();
        let m1 = try_method_one(topdir.path(), uid);
        let m2 = try_method_two(topdir.path(), uid);

        // Restore write permission so TempDir's own Drop can clean up.
        std::fs::set_permissions(topdir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

        // Root (uid 0) can write anywhere regardless of mode bits, so this
        // assertion only holds for a non-root test run -- consistent with
        // this codebase's other permission-based tests (e.g.
        // `deleter::tests::a_permission_denied_removal_surfaces_as_a_real_failure`)
        // which carry the same implicit assumption.
        if uid != 0 {
            assert!(m1.is_none());
            assert!(m2.is_none());
        }
    }

    // -- unique_trash_name ---------------------------------------------------

    #[test]
    fn unique_trash_name_uses_the_original_name_when_free() {
        let dir = TempDir::new().unwrap();
        let files = dir.path().join("files");
        let info = dir.path().join("info");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::create_dir_all(&info).unwrap();
        assert_eq!(
            unique_trash_name(&files, &info, "a.txt", &TrashReservations::new()).unwrap(),
            "a.txt"
        );
    }

    #[test]
    fn unique_trash_name_disambiguates_on_a_content_collision() {
        let dir = TempDir::new().unwrap();
        let files = dir.path().join("files");
        let info = dir.path().join("info");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(files.join("a.txt"), b"first").unwrap();
        assert_eq!(
            unique_trash_name(&files, &info, "a.txt", &TrashReservations::new()).unwrap(),
            "a (2).txt"
        );
    }

    #[test]
    fn unique_trash_name_also_checks_the_info_sidecar_not_just_content() {
        // A `.trashinfo` can exist with its content already gone (e.g. a
        // crash between the two steps -- see the module doc comment) --
        // the name is still "taken" and must not be reused.
        let dir = TempDir::new().unwrap();
        let files = dir.path().join("files");
        let info = dir.path().join("info");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("a.txt.trashinfo"), b"[Trash Info]\n").unwrap();
        assert_eq!(
            unique_trash_name(&files, &info, "a.txt", &TrashReservations::new()).unwrap(),
            "a (2).txt"
        );
    }

    #[test]
    fn unique_trash_name_keeps_incrementing_past_multiple_collisions() {
        let dir = TempDir::new().unwrap();
        let files = dir.path().join("files");
        let info = dir.path().join("info");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(files.join("a.txt"), b"1").unwrap();
        std::fs::write(files.join("a (2).txt"), b"2").unwrap();
        std::fs::write(files.join("a (3).txt"), b"3").unwrap();
        assert_eq!(
            unique_trash_name(&files, &info, "a.txt", &TrashReservations::new()).unwrap(),
            "a (4).txt"
        );
    }

    #[test]
    fn unique_trash_name_handles_a_name_with_no_extension() {
        let dir = TempDir::new().unwrap();
        let files = dir.path().join("files");
        let info = dir.path().join("info");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(files.join("Makefile"), b"x").unwrap();
        assert_eq!(
            unique_trash_name(&files, &info, "Makefile", &TrashReservations::new()).unwrap(),
            "Makefile (2)"
        );
    }

    // -- resolve_trash_destination_at (end to end within this module) -----

    #[test]
    fn home_trash_uses_an_absolute_percent_encoded_path() {
        let data_home = TempDir::new().unwrap();
        let src_dir = TempDir::new().unwrap(); // same tmpfs as data_home in this test env
        let target = src_dir.path().join("my file.txt");
        std::fs::write(&target, b"x").unwrap();

        let resolved = resolve_trash_destination_at(
            &target,
            data_home.path(),
            SystemTime::UNIX_EPOCH,
            &mut TrashReservations::new(),
        )
        .unwrap();
        assert_eq!(
            resolved.content_path,
            data_home.path().join("Trash/files/my file.txt")
        );
        assert_eq!(
            resolved.info_path,
            data_home.path().join("Trash/info/my file.txt.trashinfo")
        );
        assert!(
            resolved
                .trashinfo
                .contains(&percent_encode_path(&target.to_string_lossy())),
            "{}",
            resolved.trashinfo
        );
        assert!(resolved.trashinfo.starts_with("[Trash Info]\n"));
        assert!(resolved.trashinfo.contains("DeletionDate=1970-01-01T"));
    }

    #[test]
    fn a_second_same_named_target_gets_a_disambiguated_trashinfo_and_content_name() {
        let data_home = TempDir::new().unwrap();
        let src_dir = TempDir::new().unwrap();
        let target = src_dir.path().join("dup.txt");
        std::fs::write(&target, b"x").unwrap();

        let first = resolve_trash_destination_at(
            &target,
            data_home.path(),
            SystemTime::UNIX_EPOCH,
            &mut TrashReservations::new(),
        )
        .unwrap();
        // Simulate the first target's content having actually landed (the
        // real caller's Rename step would have done this) so the second
        // resolution sees a genuine collision.
        std::fs::write(&first.content_path, b"x").unwrap();

        let second = resolve_trash_destination_at(
            &target,
            data_home.path(),
            SystemTime::UNIX_EPOCH,
            &mut TrashReservations::new(),
        )
        .unwrap();
        assert_ne!(first.content_path, second.content_path);
        assert!(second.content_path.ends_with("dup (2).txt"));
        assert!(second.info_path.ends_with("dup (2).txt.trashinfo"));
    }

    /// The bug [`TrashReservations`] exists to prevent: two same-named
    /// targets resolved *in the same planning pass* (sharing one
    /// `TrashReservations`, the way `duet_ops::deleter::plan_trash_delete`
    /// uses it) must not both resolve to the identical destination just
    /// because neither target's content has actually moved to disk yet --
    /// planning and execution are separate phases, so a disk-only probe
    /// (what [`a_second_same_named_target_gets_a_disambiguated_trashinfo_and_content_name`]
    /// exercises, with a fresh `TrashReservations` per call and a real
    /// write in between) can't see a sibling target's not-yet-executed
    /// claim on its own.
    #[test]
    fn two_targets_resolved_in_one_planning_pass_share_reservations_and_do_not_collide() {
        let data_home = TempDir::new().unwrap();
        let src_dir_a = TempDir::new().unwrap();
        let src_dir_b = TempDir::new().unwrap();
        let target_a = src_dir_a.path().join("dup.txt");
        let target_b = src_dir_b.path().join("dup.txt");
        std::fs::write(&target_a, b"a").unwrap();
        std::fs::write(&target_b, b"b").unwrap();

        let mut reservations = TrashReservations::new();
        let first = resolve_trash_destination_at(
            &target_a,
            data_home.path(),
            SystemTime::UNIX_EPOCH,
            &mut reservations,
        )
        .unwrap();
        // Deliberately *not* performing the real Rename here -- disk state
        // stays exactly as it was before this call, proving the second
        // resolution's disambiguation comes from `reservations`, not from
        // anything newly on disk.
        let second = resolve_trash_destination_at(
            &target_b,
            data_home.path(),
            SystemTime::UNIX_EPOCH,
            &mut reservations,
        )
        .unwrap();

        assert_ne!(first.content_path, second.content_path);
        assert!(first.content_path.ends_with("dup.txt"));
        assert!(second.content_path.ends_with("dup (2).txt"));
    }

    // -- list_trash_entries (T-5.3.2 phase 1: the read side) ---------------

    /// The strongest test: a real entry [`resolve_trash_destination_at`]
    /// itself wrote, read back by [`list_trash_entries`] -- proves
    /// percent-decode and the `mktime`-based local-time-parse-back are
    /// genuine inverses of the encoder/`localtime_r`, not just
    /// independently-plausible-looking. `now` is a whole-seconds
    /// `SystemTime` (`.trashinfo` has no sub-second resolution to lose), so
    /// `deleted_at` must come back byte-for-byte identical.
    #[test]
    fn list_trash_entries_round_trips_a_real_home_trash_entry() {
        let data_home = TempDir::new().unwrap();
        let src_dir = TempDir::new().unwrap();
        let target = src_dir.path().join("déjà vu report (final).txt");
        std::fs::write(&target, b"x").unwrap();

        // An arbitrary fixed instant, deliberately not "now" -- so a test
        // that happened to pass by accident (e.g. a timezone bug that only
        // manifests for certain times of day) can't hide behind whatever
        // moment the test happened to run.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_775_000_000);
        let mut reservations = TrashReservations::new();
        let resolved =
            resolve_trash_destination_at(&target, data_home.path(), now, &mut reservations)
                .unwrap();
        // Simulate the real job: WriteTrashInfo, then Rename.
        std::fs::write(&resolved.info_path, &resolved.trashinfo).unwrap();
        std::fs::rename(&target, &resolved.content_path).unwrap();

        // list_trash_entries now also scans real per-mount trash roots
        // discovered via /proc/self/mountinfo (T-5.3.2 phase 2), so on a
        // real machine with its own real trash elsewhere the result can
        // legitimately contain more than this one entry -- find the entry
        // this test actually wrote rather than assuming it is the only
        // one / at index 0.
        let entries = list_trash_entries(data_home.path()).unwrap();
        let entry = entries
            .iter()
            .find(|e| e.content_path == resolved.content_path)
            .unwrap_or_else(|| panic!("the entry just written must be found in {entries:?}"));
        assert_eq!(entry.info_path, resolved.info_path);
        assert_eq!(
            entry.original_path, target,
            "percent-decode must exactly reverse percent_encode_path"
        );
        assert_eq!(
            entry.deleted_at, now,
            "the mktime-based inverse must exactly reverse local_civil_time"
        );
    }

    /// [`percent_decode_path`] is the exact inverse of
    /// [`percent_encode_path`] for non-ASCII bytes and spaces -- the AC's
    /// own "non-ASCII / space-containing original paths round-trip
    /// correctly" clause, isolated from the rest of the parsing pipeline.
    #[test]
    fn percent_decode_reverses_percent_encode_for_non_ascii_and_spaces() {
        let original = "/home/u/déjà vu (final) report.txt";
        let encoded = percent_encode_path(original);
        assert_eq!(percent_decode_path(&encoded).unwrap(), original);
    }

    #[test]
    fn percent_decode_rejects_a_truncated_escape() {
        assert!(percent_decode_path("abc%2").is_none());
    }

    /// A per-mount trash entry's `Path=` is stored *relative to its own
    /// trash root's topdir*, not absolute -- [`list_trash_root`]'s `Some`
    /// branch (exercised directly here, since [`list_trash_entries`]
    /// itself only ever scans home trash -- see the module doc comment's
    /// scope note) must join it back onto `topdir` to recover the real
    /// absolute `original_path`, mirroring
    /// [`resolve_trash_destination_at`]'s own `target.strip_prefix(topdir)`
    /// on the write side.
    #[test]
    fn list_trash_root_resolves_a_topdir_relative_path_for_a_per_mount_entry() {
        let topdir = TempDir::new().unwrap();
        let root = topdir.path().join(".Trash-1000");
        std::fs::create_dir_all(root.join("info")).unwrap();
        std::fs::create_dir_all(root.join("files")).unwrap();
        std::fs::write(
            root.join("info/a.txt.trashinfo"),
            "[Trash Info]\nPath=sub/dir/a.txt\nDeletionDate=2026-01-02T03:04:05\n",
        )
        .unwrap();
        std::fs::write(root.join("files/a.txt"), b"x").unwrap();

        let entries = list_trash_root(&root, Some(topdir.path())).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].original_path,
            topdir.path().join("sub/dir/a.txt")
        );
        assert_eq!(entries[0].content_path, root.join("files/a.txt"));
    }

    /// The AC's own "tolerate garbage" clause: one corrupted `.trashinfo`
    /// must not hide the other, legitimate entries in the same directory.
    #[test]
    fn list_trash_entries_skips_a_malformed_trashinfo_without_hiding_others() {
        let data_home = TempDir::new().unwrap();
        let trash_root = data_home.path().join("Trash");
        std::fs::create_dir_all(trash_root.join("info")).unwrap();
        std::fs::create_dir_all(trash_root.join("files")).unwrap();

        std::fs::write(
            trash_root.join("info/good.txt.trashinfo"),
            "[Trash Info]\nPath=/home/u/good.txt\nDeletionDate=2026-01-02T03:04:05\n",
        )
        .unwrap();
        std::fs::write(trash_root.join("files/good.txt"), b"kept").unwrap();

        // Missing the `[Trash Info]` header entirely.
        std::fs::write(
            trash_root.join("info/no-header.txt.trashinfo"),
            "Path=/home/u/no-header.txt\nDeletionDate=2026-01-02T03:04:05\n",
        )
        .unwrap();
        // Missing DeletionDate=.
        std::fs::write(
            trash_root.join("info/no-date.txt.trashinfo"),
            "[Trash Info]\nPath=/home/u/no-date.txt\n",
        )
        .unwrap();
        // Garbage, not a trashinfo file at all.
        std::fs::write(
            trash_root.join("info/garbage.txt.trashinfo"),
            "not a trashinfo file at all",
        )
        .unwrap();
        // A file in `info/` that isn't even named `*.trashinfo`.
        std::fs::write(trash_root.join("info/stray.txt"), "ignore me").unwrap();

        // Scoped to this test's own home trash (content_path under
        // data_home) -- list_trash_entries now also scans real per-mount
        // trash roots (T-5.3.2 phase 2), which on a real machine can
        // legitimately contribute their own, unrelated entries.
        let entries = list_trash_entries(data_home.path()).unwrap();
        let home_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.content_path.starts_with(data_home.path()))
            .collect();
        assert_eq!(
            home_entries.len(),
            1,
            "only the one well-formed entry should have survived: {entries:?}"
        );
        assert_eq!(
            home_entries[0].original_path,
            PathBuf::from("/home/u/good.txt")
        );
    }

    /// No `Trash` directory at all (nothing has ever been trashed here) is
    /// success with nothing found *for home trash specifically* -- mirrors
    /// [`resolve_trash_destination`]'s own "create on demand" stance on the
    /// same directory from the write side. Not asserted as "the whole
    /// result is empty": list_trash_entries now also scans real per-mount
    /// trash roots (T-5.3.2 phase 2), which on a real machine can
    /// legitimately be non-empty regardless of this test's own
    /// (freshly created, definitely-empty) `data_home`.
    #[test]
    fn list_trash_entries_on_a_data_home_with_no_trash_directory_is_an_empty_list_not_an_error() {
        let data_home = TempDir::new().unwrap();
        let entries = list_trash_entries(data_home.path()).unwrap();
        assert!(
            entries
                .iter()
                .all(|e| !e.content_path.starts_with(data_home.path())),
            "a data_home with no Trash directory must contribute no home-trash \
             entries of its own: {entries:?}"
        );
    }

    #[test]
    fn parse_deletion_date_parses_the_exact_trashinfo_shape() {
        assert_eq!(
            parse_deletion_date("2026-08-30T14:22:07"),
            Some((2026, 8, 30, 14, 22, 7))
        );
    }

    #[test]
    fn parse_deletion_date_rejects_a_malformed_value() {
        assert!(parse_deletion_date("not-a-date").is_none());
        assert!(parse_deletion_date("2026-08-30").is_none());
        assert!(parse_deletion_date("2026-08-30T14:22").is_none());
    }

    /// [`local_civil_time_to_unix`] is the exact inverse of
    /// [`local_civil_time`] -- cross-checked here independently of
    /// `list_trash_entries`'s own end-to-end round-trip test above, across
    /// a handful of distinct instants (not just one) so a bug that only
    /// shows up near a DST transition or a month/year boundary can't hide.
    #[test]
    fn local_civil_time_to_unix_is_the_exact_inverse_of_local_civil_time() {
        for secs in [0i64, 1, 1_000_000_000, 1_700_000_000, 1_800_000_000, -100] {
            let civil = local_civil_time(secs);
            let (y, mo, d, hh, mm, ss) = civil;
            let back = local_civil_time_to_unix(y, mo, d, hh, mm, ss).unwrap();
            assert_eq!(back, secs, "{civil:?} did not round-trip back to {secs}");
        }
    }

    // -- parse_mountinfo (T-5.3.2 phase 2) -----------------------------------

    #[test]
    fn parse_mountinfo_parses_a_normal_line() {
        let content =
            "36 35 98:0 / /mnt/data rw,noatime master:1 - ext4 /dev/sda1 rw,errors=remount-ro\n";
        let lines = parse_mountinfo(content);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].mount_point, PathBuf::from("/mnt/data"));
        assert_eq!(lines[0].fstype, "ext4");
    }

    #[test]
    fn parse_mountinfo_unescapes_octal_sequences_in_the_mount_point() {
        // A real removable-media mount point with a space in its volume
        // label -- `/proc/self/mountinfo` renders the space as `\040`, not
        // a literal space, precisely so the field-splitting-on-space above
        // stays unambiguous.
        let content = "50 35 8:3 / /media/USB\\040DRIVE rw - vfat /dev/sdc1 rw\n";
        let lines = parse_mountinfo(content);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].mount_point, PathBuf::from("/media/USB DRIVE"));
        assert_eq!(lines[0].fstype, "vfat");
    }

    #[test]
    fn parse_mountinfo_handles_zero_optional_fields() {
        let content = "25 30 8:1 / /boot rw - ext4 /dev/sda2 rw\n";
        let lines = parse_mountinfo(content);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].mount_point, PathBuf::from("/boot"));
        assert_eq!(lines[0].fstype, "ext4");
    }

    #[test]
    fn parse_mountinfo_handles_two_or_more_optional_fields() {
        let content = "40 35 8:2 / /mnt/backup rw shared:2 master:3 - xfs /dev/sdb1 rw\n";
        let lines = parse_mountinfo(content);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].mount_point, PathBuf::from("/mnt/backup"));
        assert_eq!(lines[0].fstype, "xfs");
    }

    #[test]
    fn parse_mountinfo_skips_a_malformed_line_without_panicking_or_hiding_good_lines() {
        let content = "36 35 98:0 / /mnt/data rw,noatime master:1 - ext4 /dev/sda1 rw\n\
                        this is not a valid mountinfo line at all\n\
                        60 35 8:4 / /mnt/x rw\n\
                        25 30 8:1 / /boot rw - ext4 /dev/sda2 rw\n";
        let lines = parse_mountinfo(content);
        assert_eq!(
            lines.len(),
            2,
            "the two malformed lines (no ' - ' separator at all) must be \
             skipped, not panic or hide the two well-formed lines: {lines:?}"
        );
        assert_eq!(lines[0].mount_point, PathBuf::from("/mnt/data"));
        assert_eq!(lines[1].mount_point, PathBuf::from("/boot"));
    }

    #[test]
    fn parse_mountinfo_on_empty_content_is_an_empty_list() {
        assert!(parse_mountinfo("").is_empty());
    }

    // -- is_probeable_fstype --------------------------------------------------

    #[test]
    fn is_probeable_fstype_excludes_pseudo_and_package_mount_filesystems() {
        for fstype in ["proc", "sysfs", "tmpfs", "overlay", "squashfs", "cgroup2"] {
            assert!(!is_probeable_fstype(fstype), "{fstype} should be excluded");
        }
    }

    #[test]
    fn is_probeable_fstype_allows_real_on_disk_filesystems() {
        for fstype in ["ext4", "btrfs", "xfs", "vfat", "ntfs3"] {
            assert!(is_probeable_fstype(fstype), "{fstype} should be probeable");
        }
    }

    // -- list_entries_from_candidate_topdirs (T-5.3.2 phase 2's pure merge) --

    #[test]
    fn list_entries_from_candidate_topdirs_finds_a_dot_trash_dash_uid_root() {
        let topdir = TempDir::new().unwrap();
        let uid = rustix::process::getuid().as_raw();
        let root = topdir.path().join(format!(".Trash-{uid}"));
        std::fs::create_dir_all(root.join("info")).unwrap();
        std::fs::create_dir_all(root.join("files")).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            root.join("info/a.txt.trashinfo"),
            "[Trash Info]\nPath=a.txt\nDeletionDate=2026-01-02T03:04:05\n",
        )
        .unwrap();
        std::fs::write(root.join("files/a.txt"), b"x").unwrap();

        let entries = list_entries_from_candidate_topdirs(&[topdir.path().to_path_buf()]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].original_path, topdir.path().join("a.txt"));
        assert_eq!(entries[0].content_path, root.join("files/a.txt"));
    }

    #[test]
    fn list_entries_from_candidate_topdirs_finds_a_sticky_bit_dot_trash_root() {
        let topdir = TempDir::new().unwrap();
        let uid = rustix::process::getuid().as_raw();
        let dot_trash = topdir.path().join(".Trash");
        std::fs::create_dir(&dot_trash).unwrap();
        std::fs::set_permissions(&dot_trash, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let user_dir = dot_trash.join(uid.to_string());
        std::fs::create_dir_all(user_dir.join("info")).unwrap();
        std::fs::create_dir_all(user_dir.join("files")).unwrap();
        std::fs::set_permissions(&user_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            user_dir.join("info/b.txt.trashinfo"),
            "[Trash Info]\nPath=b.txt\nDeletionDate=2026-01-02T03:04:05\n",
        )
        .unwrap();
        std::fs::write(user_dir.join("files/b.txt"), b"y").unwrap();

        let entries = list_entries_from_candidate_topdirs(&[topdir.path().to_path_buf()]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].original_path, topdir.path().join("b.txt"));
    }

    #[test]
    fn list_entries_from_candidate_topdirs_finds_nothing_and_creates_nothing_when_absent() {
        let topdir = TempDir::new().unwrap();
        let entries = list_entries_from_candidate_topdirs(&[topdir.path().to_path_buf()]);
        assert!(entries.is_empty());
        // The whole point of the read-only find_method_one_root/
        // find_method_two_root variants: a browse pass over a filesystem
        // with no trash on it yet must not create one as a side effect of
        // merely looking, unlike the write side's try_method_one/
        // try_method_two.
        let created: Vec<_> = std::fs::read_dir(topdir.path()).unwrap().collect();
        assert!(
            created.is_empty(),
            "a read-only browse must not create anything on disk: {created:?}"
        );
    }

    #[test]
    fn list_entries_from_candidate_topdirs_still_works_if_xdg_data_home_is_included() {
        // Per the module doc comment, this pure function doesn't know
        // about (and doesn't need to know about) the home-device exclusion
        // rule at all -- only `dedup_and_exclude_home_dev`, tested
        // separately below, does. Feeding it a path that happens to *be*
        // `xdg_data_home` should still behave correctly on its own terms.
        let data_home = TempDir::new().unwrap();
        let uid = rustix::process::getuid().as_raw();
        let root = data_home.path().join(format!(".Trash-{uid}"));
        std::fs::create_dir_all(root.join("info")).unwrap();
        std::fs::create_dir_all(root.join("files")).unwrap();
        // Mode 0700 -- required by find_existing_safe_trash_root's own
        // safety check (no group/other read/write/execute bits at all).
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            root.join("info/c.txt.trashinfo"),
            "[Trash Info]\nPath=c.txt\nDeletionDate=2026-01-02T03:04:05\n",
        )
        .unwrap();
        std::fs::write(root.join("files/c.txt"), b"z").unwrap();

        let entries = list_entries_from_candidate_topdirs(&[data_home.path().to_path_buf()]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].original_path, data_home.path().join("c.txt"));
    }

    // -- dedup_and_exclude_home_dev -------------------------------------------

    #[test]
    fn dedup_and_exclude_home_dev_drops_a_candidate_matching_xdg_data_homes_own_device() {
        let data_home = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let candidates = [data_home.path().to_path_buf(), other.path().to_path_buf()];

        let filtered = dedup_and_exclude_home_dev(data_home.path(), &candidates);

        assert!(
            !filtered.contains(&data_home.path().to_path_buf()),
            "xdg_data_home's own path must never survive the filter, since \
             it is by definition on xdg_data_home's own device: {filtered:?}"
        );
    }

    #[test]
    fn dedup_and_exclude_home_dev_deduplicates_two_candidates_on_the_same_device() {
        // Deliberately constructed rather than incidental: the exact same
        // directory listed three times always shares one st_dev by
        // definition, regardless of what /tmp happens to be mounted as in
        // whatever environment this test runs in.
        //
        // `xdg_data_home` is deliberately a nonexistent path here (so
        // `dev_of` fails and the home-device-exclusion half of this
        // function's job is a no-op via `None`) -- this test's only
        // concern is the *dedup* half; the exclusion half has its own
        // dedicated test above. Real `TempDir`s (both under the same
        // `/tmp` in most sandboxes -- see this module's own tests
        // elsewhere for the same, already-established fact) would
        // otherwise make this test's outcome depend on whether the
        // sandbox's `/tmp` and this fake home happen to share a device.
        let mount = TempDir::new().unwrap();
        let candidates = [
            mount.path().to_path_buf(),
            mount.path().to_path_buf(),
            mount.path().to_path_buf(),
        ];
        let nonexistent_home = Path::new("/definitely/does/not/exist/xdg-data-home");

        let filtered = dedup_and_exclude_home_dev(nonexistent_home, &candidates);

        assert_eq!(
            filtered.len(),
            1,
            "three candidates on the identical device must collapse to one: {filtered:?}"
        );
    }

    #[test]
    fn dedup_and_exclude_home_dev_drops_a_candidate_that_cannot_be_stated() {
        let data_home = TempDir::new().unwrap();
        let candidates = [PathBuf::from("/definitely/does/not/exist/anywhere")];
        let filtered = dedup_and_exclude_home_dev(data_home.path(), &candidates);
        assert!(filtered.is_empty());
    }

    // -- list_trash_entries_with_candidates (T-5.3.2 phase 2's end-to-end) ---

    /// The strongest phase-2 test: real content trashed via
    /// [`resolve_trash_destination_at`] into a home-trash-shaped `TempDir`,
    /// plus a second, separately-rooted `TempDir` standing in for a
    /// per-mount trash (built with the same real [`format_trashinfo`]/
    /// [`percent_encode_path`] primitives [`resolve_trash_destination_at`]
    /// itself uses, since this sandbox has no way to force a genuinely
    /// different `st_dev` without root -- see `topdir_of`'s own tests for
    /// the same, already-established constraint) -- proving entries from
    /// *both* roots show up in one [`list_trash_entries_with_candidates`]
    /// call. This is the bug from live UAT, fixed: a target trashed onto a
    /// separate mounted filesystem must actually show up in the browser.
    #[test]
    fn list_trash_entries_with_candidates_merges_home_and_per_mount_entries() {
        let data_home = TempDir::new().unwrap();
        let mount_topdir = TempDir::new().unwrap();

        // Home trash entry, via the real public write-side API.
        let src_home = TempDir::new().unwrap();
        let home_target = src_home.path().join("home-file.txt");
        std::fs::write(&home_target, b"h").unwrap();
        let mut reservations = TrashReservations::new();
        let home_resolved = resolve_trash_destination_at(
            &home_target,
            data_home.path(),
            SystemTime::UNIX_EPOCH,
            &mut reservations,
        )
        .unwrap();
        std::fs::write(&home_resolved.info_path, &home_resolved.trashinfo).unwrap();
        std::fs::rename(&home_target, &home_resolved.content_path).unwrap();

        // Per-mount trash entry: `$mount_topdir/.Trash-$uid/{files,info}`,
        // the exact layout try_method_two/find_method_two_root use, with a
        // real .trashinfo written via this module's own format_trashinfo +
        // percent_encode_path (topdir-relative Path=, matching
        // resolve_trash_destination_at's own Some(topdir) branch).
        let uid = rustix::process::getuid().as_raw();
        let mount_root = mount_topdir.path().join(format!(".Trash-{uid}"));
        std::fs::create_dir_all(mount_root.join("files")).unwrap();
        std::fs::create_dir_all(mount_root.join("info")).unwrap();
        // Mode 0700 -- required by find_existing_safe_trash_root's own
        // safety check (no group/other read/write/execute bits at all).
        std::fs::set_permissions(&mount_root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mount_src = mount_topdir.path().join("sub/mount-file.txt");
        std::fs::create_dir_all(mount_src.parent().unwrap()).unwrap();
        std::fs::write(&mount_src, b"m").unwrap();
        let relative = mount_src.strip_prefix(mount_topdir.path()).unwrap();
        let path_field = percent_encode_path(&relative.to_string_lossy());
        let civil = local_civil_time(unix_secs(SystemTime::UNIX_EPOCH));
        let trashinfo = format_trashinfo(&path_field, civil);
        std::fs::write(mount_root.join("info/mount-file.txt.trashinfo"), &trashinfo).unwrap();
        std::fs::rename(&mount_src, mount_root.join("files/mount-file.txt")).unwrap();

        let entries = list_trash_entries_with_candidates(
            data_home.path(),
            &[mount_topdir.path().to_path_buf()],
        )
        .unwrap();

        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(
            entries
                .iter()
                .any(|e| e.original_path == home_target
                    && e.content_path == home_resolved.content_path),
            "home-trash entry missing: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .any(|e| e.original_path == mount_topdir.path().join("sub/mount-file.txt")),
            "per-mount entry missing: {entries:?}"
        );
    }

    /// The public seam `duet-ui`'s tests rely on: an explicit, empty mount
    /// list must yield exactly the home trash and nothing else, no matter
    /// what the machine running the test really has mounted (this is the
    /// one variant that reads no global state at all).
    #[test]
    fn list_trash_entries_with_mounts_explicit_empty_is_home_only_and_reads_no_mount_table() {
        let data_home = TempDir::new().unwrap();
        let src_home = TempDir::new().unwrap();
        let target = src_home.path().join("only.txt");
        std::fs::write(&target, b"h").unwrap();
        let mut reservations = TrashReservations::new();
        let resolved = resolve_trash_destination_at(
            &target,
            data_home.path(),
            SystemTime::UNIX_EPOCH,
            &mut reservations,
        )
        .unwrap();
        std::fs::write(&resolved.info_path, &resolved.trashinfo).unwrap();
        std::fs::rename(&target, &resolved.content_path).unwrap();

        let entries =
            list_trash_entries_with_mounts(data_home.path(), &MountScan::Explicit(Vec::new()))
                .unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].original_path, target);

        // And the two spellings of "production" agree with each other.
        assert_eq!(MountScan::default(), MountScan::System);
        assert_eq!(
            list_trash_entries(data_home.path()).unwrap().len(),
            list_trash_entries_with_mounts(data_home.path(), &MountScan::System)
                .unwrap()
                .len()
        );
    }

    #[test]
    fn list_trash_entries_with_candidates_with_no_usable_per_mount_trash_is_home_only() {
        let data_home = TempDir::new().unwrap();
        let src_home = TempDir::new().unwrap();
        let home_target = src_home.path().join("only-file.txt");
        std::fs::write(&home_target, b"h").unwrap();
        let mut reservations = TrashReservations::new();
        let resolved = resolve_trash_destination_at(
            &home_target,
            data_home.path(),
            SystemTime::UNIX_EPOCH,
            &mut reservations,
        )
        .unwrap();
        std::fs::write(&resolved.info_path, &resolved.trashinfo).unwrap();
        std::fs::rename(&home_target, &resolved.content_path).unwrap();

        // A candidate topdir with no trash on it at all.
        let empty_mount = TempDir::new().unwrap();

        let entries = list_trash_entries_with_candidates(
            data_home.path(),
            &[empty_mount.path().to_path_buf()],
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].original_path, home_target);
    }

    /// The "defense in depth" edge case: `xdg_data_home` itself showing up
    /// in the *candidate* list must not double-count every home-trash
    /// entry. Exercised at [`dedup_and_exclude_home_dev`]'s own level (the
    /// function [`list_trash_entries`]'s real orchestration relies on for
    /// exactly this exclusion) rather than through
    /// [`list_trash_entries_with_candidates`], which deliberately skips
    /// that filter -- see that function's own doc comment for why.
    #[test]
    fn xdg_data_home_appearing_in_the_candidate_list_does_not_double_count_home_entries() {
        let data_home = TempDir::new().unwrap();
        let uid = rustix::process::getuid().as_raw();
        // If xdg_data_home's own path were (incorrectly) treated as a
        // separate per-mount candidate, this .Trash-$uid layout sitting
        // directly inside it would let a bug double-count -- but
        // dedup_and_exclude_home_dev must filter it out before it ever
        // reaches list_entries_from_candidate_topdirs.
        let shadow_root = data_home.path().join(format!(".Trash-{uid}"));
        std::fs::create_dir_all(shadow_root.join("info")).unwrap();
        std::fs::create_dir_all(shadow_root.join("files")).unwrap();
        std::fs::write(
            shadow_root.join("info/shadow.txt.trashinfo"),
            "[Trash Info]\nPath=shadow.txt\nDeletionDate=2026-01-02T03:04:05\n",
        )
        .unwrap();
        std::fs::write(shadow_root.join("files/shadow.txt"), b"s").unwrap();

        let candidates = [data_home.path().to_path_buf()];
        let filtered = dedup_and_exclude_home_dev(data_home.path(), &candidates);
        assert!(
            filtered.is_empty(),
            "xdg_data_home's own path must be excluded from the candidate \
             list before list_entries_from_candidate_topdirs ever sees it: \
             {filtered:?}"
        );

        // The real public entry point, end to end: this must not surface
        // the shadow root's entry (list_trash_entries never scans
        // $xdg_data_home/.Trash-$uid as its own home trash -- only
        // $xdg_data_home/Trash). Checked by content rather than requiring
        // the whole result to be empty, since the real
        // /proc/self/mountinfo this exercises may legitimately have other,
        // unrelated real trash entries on whatever machine runs this test.
        let entries = list_trash_entries(data_home.path()).unwrap();
        assert!(
            !entries
                .iter()
                .any(|e| e.original_path == data_home.path().join("shadow.txt")),
            "a .Trash-$uid living directly inside xdg_data_home is not \
             $xdg_data_home/Trash and must not appear: {entries:?}"
        );
    }
}
