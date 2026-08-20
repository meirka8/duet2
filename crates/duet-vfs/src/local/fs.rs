//! `LocalFs` — the `FileSystem` implementation for the real local Linux
//! filesystem. Assembled from the `local::*` submodules, each owning one
//! `T-3.1.x` slice (see their own doc comments for the design rationale).

use async_trait::async_trait;
use duet_types::{Caps, ErrorKind, MetaPatch, Metadata, Result, VPath, VfsError};
use futures_core::stream::BoxStream;
use rustix::fs::{CWD, Mode as RustixMode};

use crate::{
    AsyncReadSeek, AsyncWriteCommit, ChangeEvent, CopyOutcome, DirEntry, FileSystem, ListOpts,
    Mode, RemoveKind, RenameFlags, VolumeStats, WriteOpts,
};

use super::pathutil::real_path;
use super::readdir;
use super::statx;

/// The local Linux filesystem backend. Stateless today (no per-instance
/// configuration yet) — T-3.1.7's per-mount property cache lives in
/// `local::probe` itself (a process-wide cache keyed by `st_dev`, not
/// per-`LocalFs`-instance state, since every `LocalFs` addresses the same
/// real filesystems regardless of how many instances exist).
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalFs;

impl LocalFs {
    /// T-3.1.7: measured (not assumed) properties of the filesystem
    /// containing `dir` -- `st_dev`, rotational/reflink/case-sensitivity
    /// detection, cached per mount. Not part of the `FileSystem` trait
    /// (which has no per-path `caps()` — see `local::probe`'s module doc
    /// comment) — an inherent method on the concrete backend for callers
    /// (a future status-bar "SSD/HDD" indicator, the copy engine's
    /// strategy ladder, ...) that specifically need `LocalFs` detail
    /// beyond the trait's backend-wide `Caps`.
    pub fn probe_fs_properties(&self, dir: &VPath) -> Result<super::FsProps> {
        super::probe::probe(dir)
    }
}

#[async_trait]
impl FileSystem for LocalFs {
    fn scheme(&self) -> &'static str {
        "file"
    }

    fn caps(&self) -> Caps {
        // T-3.1.7 (filesystem-property probing) refines this per-mount;
        // today it's the conservative set every T-3.1.1/T-3.1.2-supported
        // operation actually backs.
        Caps::RANDOM_READ
            | Caps::RENAME
            | Caps::ATOMIC_REPLACE
            | Caps::HARDLINK
            | Caps::SYMLINK
            | Caps::XATTR
            | Caps::PERMISSIONS
            | Caps::TIMESTAMPS
            | Caps::CHEAP_STAT
    }

    fn read_dir(&self, p: &VPath, opts: ListOpts) -> BoxStream<'_, Result<Vec<DirEntry>>> {
        match readdir::open_dir(p) {
            Ok(fd) => readdir::stream_from_open_dir(fd, p.clone(), opts),
            Err(e) => Box::pin(futures_util::stream::once(async move { Err(e) })),
        }
    }

    async fn stat(&self, p: &VPath, follow: bool) -> Result<Metadata> {
        use crate::ListFields;
        let path = real_path(p);
        let mut md = statx::stat_one(
            CWD,
            path.to_str().unwrap_or_default(),
            ListFields::all(),
            follow,
        )
        .map_err(|e| super::pathutil::rustix_err("statx", p, e))?;
        // T-3.1.5: statx has no concept of xattrs/ACL/SELinux label;
        // enrich separately.
        super::meta::enrich(&mut md, &path, ListFields::all());
        Ok(md)
    }

    async fn volume_stats(&self, p: &VPath) -> Result<VolumeStats> {
        let path = real_path(p);
        let stats =
            rustix::fs::statvfs(&path).map_err(|e| super::pathutil::rustix_err("statvfs", p, e))?;
        // `f_bavail` (space available to an unprivileged process), not
        // `f_bfree` (raw free space, including whatever the filesystem
        // reserves for root) -- see `VolumeStats::available_bytes`'s doc
        // comment. `f_frsize` is the fragment size `statvfs`'s block
        // counts are actually denominated in (not always equal to
        // `f_bsize`).
        Ok(VolumeStats {
            total_bytes: stats.f_blocks.saturating_mul(stats.f_frsize),
            available_bytes: stats.f_bavail.saturating_mul(stats.f_frsize),
        })
    }

    async fn open_read(&self, p: &VPath) -> Result<Box<dyn AsyncReadSeek>> {
        super::rw::open_read(p).map(|f| Box::new(f) as Box<dyn AsyncReadSeek>)
    }

    async fn open_write(&self, p: &VPath, o: WriteOpts) -> Result<Box<dyn AsyncWriteCommit>> {
        super::rw::open_write(p, o).map(|h| Box::new(h) as Box<dyn AsyncWriteCommit>)
    }

    async fn create_dir(&self, p: &VPath, mode: Option<Mode>) -> Result<()> {
        super::guard::assert_not_ui_thread();
        let path = real_path(p);
        let m = RustixMode::from_raw_mode(mode.unwrap_or(Mode::DEFAULT_DIR).bits());
        rustix::fs::mkdirat(CWD, &path, m).map_err(|e| super::pathutil::rustix_err("mkdirat", p, e))
    }

    async fn remove(&self, p: &VPath, kind: RemoveKind) -> Result<()> {
        match kind {
            RemoveKind::Recursive => super::traverse::remove_recursive(p),
            RemoveKind::File | RemoveKind::EmptyDir => {
                super::guard::assert_not_ui_thread();
                let path = real_path(p);
                let flags = if kind == RemoveKind::EmptyDir {
                    rustix::fs::AtFlags::REMOVEDIR
                } else {
                    rustix::fs::AtFlags::empty()
                };
                rustix::fs::unlinkat(CWD, &path, flags)
                    .map_err(|e| super::pathutil::rustix_err("unlinkat", p, e))
            }
        }
    }

    async fn rename(&self, from: &VPath, to: &VPath, flags: RenameFlags) -> Result<()> {
        super::guard::assert_not_ui_thread();
        let from_path = real_path(from);
        let to_path = real_path(to);
        let mut rflags = rustix::fs::RenameFlags::empty();
        if flags.contains(RenameFlags::NO_REPLACE) {
            rflags |= rustix::fs::RenameFlags::NOREPLACE;
        }
        if flags.contains(RenameFlags::EXCHANGE) {
            rflags |= rustix::fs::RenameFlags::EXCHANGE;
        }
        rustix::fs::renameat_with(CWD, &from_path, CWD, &to_path, rflags)
            .map_err(|e| super::pathutil::rustix_err("renameat2", from, e))
    }

    async fn link(&self, source: &VPath, dest: &VPath) -> Result<()> {
        super::guard::assert_not_ui_thread();
        let source_path = real_path(source);
        let dest_path = real_path(dest);
        // No `AtFlags::SYMLINK_FOLLOW`: `source` is expected to be a
        // regular file this backend itself created (T-5.1.7's hardlink-
        // graph dedup, or an explicit non-symlink "create hardlink"
        // request), never a symlink to follow.
        rustix::fs::linkat(
            CWD,
            &source_path,
            CWD,
            &dest_path,
            rustix::fs::AtFlags::empty(),
        )
        .map_err(|e| super::pathutil::rustix_err("linkat", source, e))
    }

    async fn set_meta(&self, p: &VPath, m: &MetaPatch) -> Result<()> {
        super::meta::set_meta(p, m)
    }

    fn watch(&self, p: &VPath) -> Result<BoxStream<'_, ChangeEvent>> {
        Err(Box::new(
            VfsError::new(
                ErrorKind::Fatal,
                "LocalFs does not yet implement watch (Caps::WATCH absent)",
            )
            .with_path(p.clone()),
        ))
    }

    async fn server_side_copy(
        &self,
        from: &VPath,
        to: &VPath,
        on_progress: &(dyn Fn(u64) -> bool + Send + Sync),
    ) -> Result<CopyOutcome> {
        super::probe::accelerated_copy(from, to, on_progress)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;
    use std::sync::Arc;

    use duet_types::UnixPathBuf;
    use futures_util::StreamExt;
    use tempfile::TempDir;

    use super::*;

    fn vp(dir: &TempDir, name: &str) -> VPath {
        VPath::local(UnixPathBuf::new(&format!("{}/{}", dir.path().display(), name)).unwrap())
    }

    #[test]
    fn constructs_as_boxed_and_arced_trait_object() {
        let _boxed: Box<dyn FileSystem> = Box::new(LocalFs);
        let _arced: Arc<dyn FileSystem> = Arc::new(LocalFs);
    }

    #[tokio::test]
    async fn read_dir_end_to_end_through_the_trait() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hi").unwrap();
        let root = VPath::local(UnixPathBuf::new(dir.path().to_str().unwrap()).unwrap());
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let mut stream = fs.read_dir(&root, ListOpts::names_only());
        let mut names = vec![];
        while let Some(chunk) = stream.next().await {
            for e in chunk.unwrap() {
                names.push(e.name);
            }
        }
        assert_eq!(names, vec!["a.txt".to_string()]);
    }

    #[tokio::test]
    async fn stat_reports_size_and_kind() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let md = fs.stat(&vp(&dir, "a.txt"), false).await.unwrap();
        assert_eq!(md.size, 5);
        assert_eq!(md.kind, duet_types::EntryKind::File);
    }

    #[tokio::test]
    async fn stat_missing_path_is_not_found() {
        let dir = TempDir::new().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let err = fs.stat(&vp(&dir, "missing"), false).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn create_dir_then_remove_round_trips() {
        let dir = TempDir::new().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let target = vp(&dir, "newdir");
        fs.create_dir(&target, None).await.unwrap();
        assert!(dir.path().join("newdir").is_dir());
        fs.remove(&target, RemoveKind::EmptyDir).await.unwrap();
        assert!(!dir.path().join("newdir").exists());
    }

    #[tokio::test]
    async fn rename_moves_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("old.txt"), b"x").unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        fs.rename(
            &vp(&dir, "old.txt"),
            &vp(&dir, "new.txt"),
            RenameFlags::empty(),
        )
        .await
        .unwrap();
        assert!(!dir.path().join("old.txt").exists());
        assert!(dir.path().join("new.txt").exists());
    }

    #[tokio::test]
    async fn rename_no_replace_conflicts_on_existing_destination() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let err = fs
            .rename(
                &vp(&dir, "a.txt"),
                &vp(&dir, "b.txt"),
                RenameFlags::NO_REPLACE,
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Conflict);
    }

    #[tokio::test]
    async fn link_creates_a_second_name_for_the_same_inode() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"shared content").unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);

        fs.link(&vp(&dir, "a.txt"), &vp(&dir, "b.txt"))
            .await
            .unwrap();

        let a_meta = std::fs::metadata(dir.path().join("a.txt")).unwrap();
        let b_meta = std::fs::metadata(dir.path().join("b.txt")).unwrap();
        assert_eq!(
            a_meta.ino(),
            b_meta.ino(),
            "both names must resolve to the same inode"
        );
        assert_eq!(a_meta.nlink(), 2);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "shared content"
        );

        // Writing through either name must be visible through the other --
        // they share one inode, not two copies of the same bytes.
        std::fs::write(dir.path().join("a.txt"), b"changed").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "changed"
        );
    }

    #[tokio::test]
    async fn link_reports_not_found_for_a_missing_source() {
        let dir = TempDir::new().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let err = fs
            .link(&vp(&dir, "missing.txt"), &vp(&dir, "b.txt"))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn link_reports_conflict_for_an_existing_destination() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let err = fs
            .link(&vp(&dir, "a.txt"), &vp(&dir, "b.txt"))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Conflict);
    }

    #[tokio::test]
    async fn server_side_copy_on_tmpfs_copies_content() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("src.txt"), b"copy me").unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let outcome = fs
            .server_side_copy(&vp(&dir, "src.txt"), &vp(&dir, "dst.txt"), &|_| false)
            .await
            .unwrap();
        match outcome {
            CopyOutcome::Copied { bytes, reflinked } => {
                assert_eq!(bytes, 7);
                // tmpfs does not implement FICLONE (confirmed by
                // local::probe::tests::probes_tmpfs_correctly); this must
                // have gone through the copy_file_range fallback.
                assert!(!reflinked);
            }
            other => panic!("expected an accelerated copy on tmpfs, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(dir.path().join("dst.txt")).unwrap(),
            b"copy me"
        );
    }

    #[tokio::test]
    async fn server_side_copy_conflicts_when_destination_exists() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("src.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("dst.txt"), b"already here").unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFs);
        let err = fs
            .server_side_copy(&vp(&dir, "src.txt"), &vp(&dir, "dst.txt"), &|_| false)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Conflict);
        assert_eq!(
            std::fs::read(dir.path().join("dst.txt")).unwrap(),
            b"already here"
        );
    }

    #[test]
    fn probe_fs_properties_reports_tmpfs() {
        let dir = TempDir::new().unwrap();
        let root = VPath::local(UnixPathBuf::new(dir.path().to_str().unwrap()).unwrap());
        let fs = LocalFs;
        let props = fs.probe_fs_properties(&root).unwrap();
        assert_eq!(props.kind, crate::local::FsKind::Tmpfs);
    }
}
