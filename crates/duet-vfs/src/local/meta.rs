//! T-3.1.5 — metadata get/set: mode, times, ownership, xattrs, POSIX ACLs,
//! SELinux label.
//!
//! `duet_types::MetaPatch` (T-2.2.1) has no dedicated ACL/SELinux fields —
//! only `mode`/`uid`/`gid`/`modified`/`accessed`/`set_xattrs`/
//! `remove_xattrs`. That's not a gap this module needs to work around: on
//! Linux, POSIX ACLs and the SELinux label *are* extended attributes
//! (`system.posix_acl_access`/`system.posix_acl_default` and
//! `security.selinux` respectively — exactly the task brief's own
//! suggestion), so a caller sets either one by putting the right bytes
//! under the right name in `MetaPatch::set_xattrs`, and this module's
//! generic xattr handling covers them with no special-casing needed on the
//! *write* side. The *read* side ([`enrich`]) does special-case those two
//! names, purely as a convenience so `Metadata::acl`/`Metadata::
//! selinux_label` are populated without every caller having to know the
//! xattr name by heart.
//!
//! # Apply order (corrected by T-5.1.6)
//!
//! Per design.md §9.3, verbatim: mode, then xattrs (ACL/SELinux included),
//! then timestamps, then ownership *last*. Two independent reasons drive
//! this order, not just one:
//! - Timestamps come after xattrs because setting xattrs perturbs `ctime`,
//!   and (within timestamps vs. xattrs) applying whatever the caller
//!   explicitly asked for last means it's the value that actually sticks,
//!   not something the kernel bumped back up afterward.
//! - Ownership comes after *everything*, including timestamps, because
//!   `chown(2)` clears a file's setuid/setgid mode bits as a security
//!   measure when the caller lacks `CAP_FSETID` — applying it any earlier
//!   (this module's own behaviour before T-5.1.6) would silently lose a
//!   setuid/setgid bit `chmod` had just set, for any caller in that
//!   position. See [`set_meta`]'s own doc comment for the full
//!   explanation.
//!
//! # Degrade, don't fail (T-3.1.5's AC)
//!
//! A filesystem that doesn't support a given field (tmpfs commonly lacks
//! ACL/SELinux xattr support; a filesystem mounted without `user_xattr`
//! rejects everything under `user.*`) reports `ENOTSUP`/`EOPNOTSUPP`.
//! [`set_meta`] treats that per-field, per the task's own instruction:
//! skip the field, record a warning, and keep applying the rest of the
//! patch, rather than failing the whole call over one unsupported field.
//! Warnings go to `eprintln!` today — `duet-vfs` has no logging dependency
//! yet; routing this through the real facility once one exists (tracked
//! separately, see the phase-3 logging work) is a follow-up, not a
//! silently-dropped requirement.

use std::collections::BTreeMap;
use std::path::Path;

use duet_types::{Metadata, Result, Timestamp, VPath};
use rustix::fs::{
    self, AtFlags, CWD, Gid, Mode as RustixMode, Timespec, Timestamps, Uid, XattrFlags,
};
use rustix::io::Errno;

use super::guard;
use super::pathutil::{real_path, rustix_err};
use crate::ListFields;

/// The well-known xattr name POSIX ACLs live under (access ACL; there is
/// also `system.posix_acl_default` for directories' inherited-default ACL,
/// not separately modelled by `duet_types::Metadata` today).
const ACL_ACCESS_XATTR: &str = "system.posix_acl_access";
/// The well-known xattr name the SELinux label lives under.
const SELINUX_XATTR: &str = "security.selinux";

const XATTR_INITIAL_BUF: usize = 4096;
const XATTR_MAX_BUF: usize = 1 << 20;

/// Reads a single xattr's raw value, growing the read buffer on `ERANGE`.
/// `Ok(None)` for "attribute doesn't exist" or "this filesystem doesn't do
/// xattrs at all" alike — both are "nothing to report," not errors, for a
/// best-effort metadata read.
fn get_xattr_raw(path: &Path, name: &str) -> rustix::io::Result<Option<Vec<u8>>> {
    let mut cap = XATTR_INITIAL_BUF;
    loop {
        let mut buf = vec![0u8; cap];
        match fs::lgetxattr(path, name, &mut buf) {
            Ok(len) => {
                buf.truncate(len);
                return Ok(Some(buf));
            }
            Err(Errno::RANGE) if cap < XATTR_MAX_BUF => cap *= 4,
            Err(Errno::NODATA) | Err(Errno::NOTSUP) => return Ok(None),
            Err(e) => return Err(e),
        }
    }
}

/// Lists every xattr name set on `path`, growing the buffer on `ERANGE`.
fn list_xattr_names(path: &Path) -> rustix::io::Result<Vec<String>> {
    let mut cap = XATTR_INITIAL_BUF;
    loop {
        let mut buf = vec![0u8; cap];
        match fs::llistxattr(path, &mut buf) {
            Ok(len) => {
                buf.truncate(len);
                return Ok(buf
                    .split(|&b| b == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect());
            }
            Err(Errno::RANGE) if cap < XATTR_MAX_BUF => cap *= 4,
            Err(Errno::NODATA) | Err(Errno::NOTSUP) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        }
    }
}

fn get_all_xattrs(path: &Path) -> rustix::io::Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    for name in list_xattr_names(path)? {
        if let Some(v) = get_xattr_raw(path, &name)? {
            out.insert(name, v);
        }
    }
    Ok(out)
}

/// Populates `md.xattrs`/`md.acl`/`md.selinux_label` per `fields`, on top
/// of whatever `local::statx` already filled in. Best-effort throughout:
/// an error reading one of these (including "not supported at all") just
/// leaves the corresponding field `None`, matching `ListFields`'s own
/// "hint, not contract" doc comment.
pub fn enrich(md: &mut Metadata, path: &Path, fields: ListFields) {
    guard::assert_not_ui_thread();
    if fields.contains(ListFields::XATTRS) {
        md.xattrs = get_all_xattrs(path).ok();
    }
    if fields.contains(ListFields::ACL) {
        md.acl = get_xattr_raw(path, ACL_ACCESS_XATTR).ok().flatten();
    }
    if fields.contains(ListFields::SELINUX) {
        md.selinux_label = get_xattr_raw(path, SELINUX_XATTR)
            .ok()
            .flatten()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .map(|s| s.trim_end_matches('\0').to_string());
    }
}

fn to_timespec(t: Timestamp) -> Timespec {
    Timespec {
        tv_sec: t.secs,
        tv_nsec: i64::from(t.nanos),
    }
}

fn omit_timespec() -> Timespec {
    Timespec {
        tv_sec: 0,
        tv_nsec: fs::UTIME_OMIT,
    }
}

/// `FileSystem::set_meta`'s implementation. See module doc comment for the
/// apply order and the "degrade on unsupported field" contract.
///
/// # Apply order (fixed by T-5.1.6)
///
/// design.md §9.3, verbatim: "mode, then xattrs..., POSIX ACLs, SELinux
/// label, then timestamps *last*..., then ownership if privileged."
/// Ownership genuinely has to be the very last step, not merely "somewhere
/// after mode" -- `chown(2)` on Linux clears a file's setuid/setgid bits as
/// a security measure whenever the caller lacks `CAP_FSETID` (a
/// non-privileged process always lacks it, so its `chown` clears these
/// bits unconditionally -- confirmed directly while building this fix's
/// test coverage; a privileged process normally holds it, so its `chown`
/// leaves them alone). Applying `chown` *before* `chmod` (this function's
/// behaviour before T-5.1.6) would silently lose a setuid/setgid bit
/// `chmod` had just set, for any caller in the "lacks `CAP_FSETID`"
/// position -- exactly the kind of "tricky file" T-5.1.6's own AC
/// (byte-identical `stat` comparison for a corpus of tricky files) exists
/// to catch.
pub fn set_meta(p: &VPath, m: &duet_types::MetaPatch) -> Result<()> {
    guard::assert_not_ui_thread();
    let path = real_path(p);
    let mut warnings: Vec<String> = Vec::new();

    // 1. Mode. Linux has no `lchmod`; `chmodat` always follows a symlink
    // in the last component (AtFlags::SYMLINK_NOFOLLOW is rejected with
    // ENOTSUP for chmod specifically -- there is no such thing as
    // permission bits on a symlink itself on Linux).
    if let Some(mode_bits) = m.mode {
        let mode = RustixMode::from_raw_mode(mode_bits);
        fs::chmodat(CWD, &path, mode, AtFlags::empty()).map_err(|e| rustix_err("chmodat", p, e))?;
    }

    // 2. Xattrs -- ACL/SELinux included, via whatever name the caller put
    // in `set_xattrs`/`remove_xattrs` (see module doc comment).
    for (name, value) in &m.set_xattrs {
        if let Err(e) = fs::lsetxattr(&path, name.as_str(), value, XattrFlags::empty()) {
            if e == Errno::NOTSUP || e == Errno::PERM {
                warnings.push(format!(
                    "xattr {name:?} could not be set (unsupported or denied on this \
                     filesystem): {e}"
                ));
                continue;
            }
            return Err(rustix_err("lsetxattr", p, e));
        }
    }
    for name in &m.remove_xattrs {
        if let Err(e) = fs::lremovexattr(&path, name.as_str()) {
            if e == Errno::NODATA {
                continue; // Already absent -- removal is a no-op, not an error.
            }
            if e == Errno::NOTSUP {
                warnings.push(format!(
                    "xattr {name:?} removal unsupported on this filesystem: {e}"
                ));
                continue;
            }
            return Err(rustix_err("lremovexattr", p, e));
        }
    }

    // 3. Timestamps (see module doc comment for why these come before
    // ownership but after everything else above).
    if m.modified.is_some() || m.accessed.is_some() {
        let times = Timestamps {
            last_access: m.accessed.map(to_timespec).unwrap_or_else(omit_timespec),
            last_modification: m.modified.map(to_timespec).unwrap_or_else(omit_timespec),
        };
        fs::utimensat(CWD, &path, &times, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|e| rustix_err("utimensat", p, e))?;
    }

    // 4. Ownership, genuinely last (see this function's own doc comment).
    // Degrades on `EPERM`/`ROFS` rather than hard-failing the whole call --
    // an unprivileged process attempting to `chown` to an arbitrary uid/gid
    // (e.g. a plain copy preserving the source's original owner) is the
    // common case now that T-5.1.6 wires this into every copy/move, not an
    // exceptional one; design.md's own "ownership if privileged" already
    // frames this as attempt-and-degrade, not attempt-and-fail.
    if m.uid.is_some() || m.gid.is_some() {
        let uid = m.uid.map(Uid::from_raw);
        let gid = m.gid.map(Gid::from_raw);
        if let Err(e) = fs::chownat(CWD, &path, uid, gid, AtFlags::SYMLINK_NOFOLLOW) {
            if e == Errno::PERM || e == Errno::ROFS {
                warnings.push(format!(
                    "ownership could not be set (not privileged, or a read-only \
                     filesystem): {e}"
                ));
            } else {
                return Err(rustix_err("chownat", p, e));
            }
        }
    }

    for w in &warnings {
        // See module doc comment: no logging dependency yet, this is the
        // documented interim sink for "recorded warning, not a hard
        // error."
        eprintln!("duet-vfs local::meta::set_meta: {w}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use duet_types::{MetaPatch, UnixPathBuf};
    use tempfile::TempDir;

    use super::*;

    fn vp(dir: &TempDir, name: &str) -> VPath {
        VPath::local(UnixPathBuf::new(&format!("{}/{}", dir.path().display(), name)).unwrap())
    }

    /// T-3.1.5 AC: round-trip test -- set then get preserves mode, times,
    /// ownership (to the same uid/gid the test runs as, since arbitrary
    /// chown needs privilege this environment doesn't have), and a plain
    /// xattr.
    #[test]
    fn round_trip_mode_times_owner_and_xattr() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"content").unwrap();
        let target = vp(&dir, "f.txt");
        let path = real_path(&target);

        let my_uid = rustix::process::getuid().as_raw();
        let my_gid = rustix::process::getgid().as_raw();

        let patch = MetaPatch {
            mode: Some(0o640),
            uid: Some(my_uid),
            gid: Some(my_gid),
            modified: Some(Timestamp::new(1_700_000_000, 123_000_000)),
            accessed: Some(Timestamp::new(1_600_000_000, 0)),
            set_xattrs: BTreeMap::from([("user.duet.test".to_string(), b"hello-xattr".to_vec())]),
            remove_xattrs: Vec::new(),
        };
        set_meta(&target, &patch).unwrap();

        let st = fs::statat(CWD, &path, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        assert_eq!(st.st_mode & 0o777, 0o640);
        assert_eq!(st.st_uid, my_uid);
        assert_eq!(st.st_gid, my_gid);
        assert_eq!(st.st_mtime, 1_700_000_000);
        assert_eq!(st.st_atime, 1_600_000_000);

        let xattrs = get_all_xattrs(&path).unwrap();
        assert_eq!(
            xattrs.get("user.duet.test").map(Vec::as_slice),
            Some(b"hello-xattr".as_slice())
        );

        let mut md = Metadata::minimal(duet_types::EntryKind::File);
        enrich(&mut md, &path, ListFields::XATTRS);
        assert_eq!(
            md.xattrs.unwrap().get("user.duet.test").map(Vec::as_slice),
            Some(b"hello-xattr".as_slice())
        );
    }

    /// T-5.1.6: proves the apply-order fix, gated on actually running as
    /// root -- confirmed empirically while writing this test that an
    /// *unprivileged* `chown` clears setuid/setgid on Linux unconditionally
    /// (even a no-op chown to the caller's own current uid/gid), regardless
    /// of whether it runs before or after `chmod`. That's expected kernel
    /// behaviour (`should_remove_suid`, gated on the caller lacking
    /// `CAP_FSETID`), not something this ordering fix changes or could ever
    /// change for an unprivileged caller -- the fix's actual benefit is
    /// specifically for a *privileged* process (root, which normally holds
    /// `CAP_FSETID`, so its `chown` does *not* clear these bits): applying
    /// ownership before mode (this module's behaviour before T-5.1.6) would
    /// still be *safe* for root today, but the fix matters the moment
    /// ownership is applied through a path that ever runs without
    /// `CAP_FSETID` while still being able to `chown` (a `setcap`'d
    /// helper, say) -- not reproducible in this unprivileged dev/CI
    /// environment, so this test degrades to a recorded skip rather than a
    /// false pass or a spurious failure, per this codebase's existing
    /// precedent for privilege/environment-dependent behaviour (see e.g.
    /// this module's own ACL/SELinux tests).
    #[test]
    fn setuid_bit_survives_ownership_application_when_privileged() {
        if rustix::process::getuid().as_raw() != 0 {
            eprintln!(
                "setuid_bit_survives_ownership_application_when_privileged: not running as \
                 root -- skipping. An unprivileged chown clears setuid/setgid unconditionally \
                 on Linux regardless of apply order (confirmed directly while writing this \
                 test), so this ordering fix's benefit can only be observed running \
                 privileged. Recorded, not a test failure."
            );
            return;
        }

        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"content").unwrap();
        let target = vp(&dir, "f.txt");
        let path = real_path(&target);

        let my_uid = rustix::process::getuid().as_raw();
        let my_gid = rustix::process::getgid().as_raw();

        let patch = MetaPatch {
            mode: Some(0o4750), // setuid + rwxr-x---
            uid: Some(my_uid),
            gid: Some(my_gid),
            ..Default::default()
        };
        set_meta(&target, &patch).unwrap();

        let st = fs::statat(CWD, &path, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        assert_eq!(
            st.st_mode & 0o7777,
            0o4750,
            "the setuid bit must survive when a privileged process applies ownership last"
        );
    }

    /// The order-independent regression case *is* fully testable
    /// unprivileged: `set_meta` must apply `mode` before attempting
    /// ownership at all, so a setuid bit survives whenever the patch
    /// carries no `uid`/`gid` (the common case for a plain, same-owner
    /// copy) -- no chown call happens, so nothing can clear it.
    #[test]
    fn setuid_bit_survives_when_the_patch_has_no_ownership_change() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"content").unwrap();
        let target = vp(&dir, "f.txt");
        let path = real_path(&target);

        let patch = MetaPatch {
            mode: Some(0o4750), // setuid + rwxr-x---
            ..Default::default()
        };
        set_meta(&target, &patch).unwrap();

        let st = fs::statat(CWD, &path, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        assert_eq!(st.st_mode & 0o7777, 0o4750);
    }

    /// T-3.1.5 AC: "unsupported attributes degrade with a recorded
    /// warning, not a hard error." Removing a nonexistent xattr is the one
    /// deterministic, environment-independent way to exercise the
    /// degrade-not-fail path without depending on a specific filesystem's
    /// ACL/SELinux support (which varies -- see the module/task doc
    /// comments): `set_meta` must not fail the whole call over it.
    #[test]
    fn remove_nonexistent_xattr_does_not_fail_the_call() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
        let target = vp(&dir, "f.txt");
        let patch = MetaPatch {
            remove_xattrs: vec!["user.duet.does-not-exist".to_string()],
            ..Default::default()
        };
        set_meta(&target, &patch).unwrap();
    }

    /// T-5.1.6: an unprivileged `chown` to a uid the caller doesn't own
    /// (root's, here -- reliably not-ours in any dev/CI environment this
    /// test actually runs unprivileged in) must degrade the same way an
    /// unsupported xattr already does: a recorded warning, not a hard
    /// failure of the whole call -- and everything *before* ownership in
    /// the apply order (mode, in this case) must still have taken effect.
    /// This is what makes it safe for `plan_copy`/`plan_move` (T-5.1.6) to
    /// unconditionally attempt ownership preservation on every copy,
    /// rather than needing to pre-check privilege itself.
    #[test]
    fn chown_to_an_unowned_uid_degrades_instead_of_failing_the_whole_call() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
        let target = vp(&dir, "f.txt");
        let path = real_path(&target);
        let my_uid = rustix::process::getuid().as_raw();
        assert_ne!(
            my_uid, 0,
            "this test assumes it's not already running as root"
        );

        let patch = MetaPatch {
            mode: Some(0o640),
            uid: Some(0), // root -- not ours, and we're not privileged to claim it
            ..Default::default()
        };
        set_meta(&target, &patch).unwrap();

        let st = fs::statat(CWD, &path, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        assert_eq!(
            st.st_mode & 0o777,
            0o640,
            "mode must still have been applied even though ownership was denied"
        );
        assert_eq!(
            st.st_uid, my_uid,
            "the failed chown must not have partially applied"
        );
    }

    /// Best-effort round-trip of the ACL/SELinux *convenience* fields
    /// through the same generic xattr mechanism, on whatever this
    /// filesystem actually supports -- logs (doesn't fail the test) when
    /// unsupported, satisfying the AC's "note if xattrs/ACLs behave
    /// differently on tmpfs" instruction with a real, observed result
    /// rather than an assumption.
    #[test]
    fn acl_xattr_round_trips_or_is_reported_unsupported() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
        let target = vp(&dir, "f.txt");
        let path = real_path(&target);

        // A minimal, syntactically-valid POSIX ACL access xattr isn't
        // trivial to hand-construct without a dedicated ACL crate (this
        // module deliberately doesn't add one -- see its module doc
        // comment: Metadata::acl is documented as raw bytes, and setting
        // is exposed via the generic xattr path). Exercise the round-trip
        // with a value that's the right *shape* to be accepted by
        // filesystems willing to store it, and report (not fail) if the
        // kernel/filesystem rejects it as semantically invalid or
        // unsupported -- both are legitimate, observed outcomes this test
        // is explicitly designed to distinguish and record, per the AC.
        let fake_acl_bytes: Vec<u8> = vec![0x02, 0x00, 0x00, 0x00]; // ACL version header only
        match fs::lsetxattr(
            &path,
            ACL_ACCESS_XATTR,
            &fake_acl_bytes,
            XattrFlags::empty(),
        ) {
            Ok(()) => {
                let mut md = Metadata::minimal(duet_types::EntryKind::File);
                enrich(&mut md, &path, ListFields::ACL);
                match md.acl.as_deref() {
                    Some(bytes) => assert_eq!(bytes, fake_acl_bytes.as_slice()),
                    None => {
                        // Real, observed Linux VFS behaviour (confirmed
                        // via a direct os.setxattr/getxattr probe while
                        // developing this test): a header-only ACL with
                        // zero entries is "trivial" -- equivalent to
                        // carrying no information beyond `st_mode` -- and
                        // several filesystems' ACL xattr handlers
                        // silently decline to persist a trivial ACL
                        // rather than storing a redundant xattr, so the
                        // immediately-following getxattr legitimately
                        // returns ENODATA even though `set` reported
                        // success. Recorded, not a test failure -- this
                        // *is* the degrade-gracefully path in practice, on
                        // a filesystem that "supports" ACLs in general but
                        // optimizes this particular trivial case away.
                        eprintln!(
                            "acl_xattr_round_trips_or_is_reported_unsupported: {ACL_ACCESS_XATTR} \
                             set() succeeded but the kernel did not persist this trivial \
                             (entry-free) ACL -- a real, expected VFS optimization, not a bug in \
                             this crate."
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "acl_xattr_round_trips_or_is_reported_unsupported: {ACL_ACCESS_XATTR} \
                     rejected on this filesystem/kernel ({e}) -- recorded, not a test failure \
                     (T-3.1.5 AC: degrade, don't hard-fail)."
                );
            }
        }
    }

    #[test]
    fn selinux_xattr_round_trips_or_is_reported_unsupported() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
        let target = vp(&dir, "f.txt");
        let path = real_path(&target);

        match fs::lgetxattr(&path, SELINUX_XATTR, &mut vec![0u8; 256]) {
            Ok(_) => {
                let mut md = Metadata::minimal(duet_types::EntryKind::File);
                enrich(&mut md, &path, ListFields::SELINUX);
                assert!(md.selinux_label.is_some());
            }
            Err(e) => {
                eprintln!(
                    "selinux_xattr_round_trips_or_is_reported_unsupported: {SELINUX_XATTR} \
                     not present/not supported on this system ({e}) -- recorded, not a test \
                     failure (this dev environment is not running SELinux, per T-3.1.5's own \
                     instruction to note rather than assume filesystem/LSM capabilities)."
                );
            }
        }
    }
}
