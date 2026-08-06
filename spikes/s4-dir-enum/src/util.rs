//! Shared helpers: directory-fd opening and raw name listing via getdents64.

use std::ffi::CString;
use std::mem::MaybeUninit;
use std::path::Path;

use rustix::fd::OwnedFd;
use rustix::fs::{self, FileType, Mode, OFlags, RawDir, CWD};

/// Open a directory relative to CWD and return an owned fd, per the
/// "operate relative to a directory fd" pattern (`*at` syscalls) rather than
/// building fresh absolute paths for every entry.
pub fn open_dir(path: &Path) -> OwnedFd {
    fs::openat(
        CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap_or_else(|e| panic!("openat({}) failed: {e}", path.display()))
}

/// Directory-entry name + d_type, as delivered by getdents64 with no
/// additional syscalls.
pub struct RawEntry {
    pub name: CString,
    pub d_type: FileType,
}

/// Drain every entry out of a directory fd via raw `getdents64`, skipping
/// `.` and `..`. This is the shared first phase for all three strategies:
/// it is the unavoidable cost of enumeration itself.
pub fn list_entries(dir_fd: &OwnedFd) -> Vec<RawEntry> {
    let mut buf = vec![MaybeUninit::uninit(); 1 << 20]; // 1 MiB getdents buffer
    let mut iter = RawDir::new(dir_fd, buf.as_mut_slice());
    let mut out = Vec::new();
    while let Some(entry) = iter.next() {
        let entry = entry.expect("getdents64 failed");
        let name = entry.file_name();
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        out.push(RawEntry {
            name: name.to_owned(),
            d_type: entry.file_type(),
        });
    }
    out
}
