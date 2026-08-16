//! `NullFs` — a trivial, always-empty-or-always-fails [`FileSystem`]
//! implementation.
//!
//! Exists purely to prove the trait shape is actually implementable and
//! object-safe (T-2.2.2's acceptance criterion), not as a real backend.
//! Every read-style method reports `NotFound` (there is never anything to
//! find); every mutation reports `Fatal` (there is nothing to mutate);
//! `server_side_copy` reports `Unsupported` (the one method where "can't do
//! this" is a normal `Ok`, not an error — see [`CopyOutcome`]).

use async_trait::async_trait;
use duet_types::{Caps, ErrorKind, MetaPatch, Metadata, Result, VPath, VfsError};
use futures_core::stream::BoxStream;

use crate::{
    AsyncReadSeek, AsyncWriteCommit, ChangeEvent, CopyOutcome, DirEntry, FileSystem, ListOpts,
    Mode, RemoveKind, RenameFlags, VolumeStats, WriteOpts,
};

/// A `FileSystem` with no entries and no ability to create any.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullFs;

fn not_found(p: &VPath) -> Box<VfsError> {
    Box::new(VfsError::new(ErrorKind::NotFound, "NullFs has no entries").with_path(p.clone()))
}

fn unsupported(p: &VPath) -> Box<VfsError> {
    Box::new(
        VfsError::new(ErrorKind::Fatal, "NullFs does not support mutation").with_path(p.clone()),
    )
}

#[async_trait]
impl FileSystem for NullFs {
    fn scheme(&self) -> &'static str {
        "null"
    }

    fn caps(&self) -> Caps {
        Caps::empty()
    }

    fn read_dir(&self, _p: &VPath, _opts: ListOpts) -> BoxStream<'_, Result<Vec<DirEntry>>> {
        Box::pin(futures_util::stream::empty())
    }

    async fn stat(&self, p: &VPath, _follow: bool) -> Result<Metadata> {
        Err(not_found(p))
    }

    async fn volume_stats(&self, p: &VPath) -> Result<VolumeStats> {
        Err(not_found(p))
    }

    async fn open_read(&self, p: &VPath) -> Result<Box<dyn AsyncReadSeek>> {
        Err(not_found(p))
    }

    async fn open_write(&self, p: &VPath, _o: WriteOpts) -> Result<Box<dyn AsyncWriteCommit>> {
        Err(unsupported(p))
    }

    async fn create_dir(&self, p: &VPath, _mode: Option<Mode>) -> Result<()> {
        Err(unsupported(p))
    }

    async fn remove(&self, p: &VPath, _kind: RemoveKind) -> Result<()> {
        Err(not_found(p))
    }

    async fn rename(&self, from: &VPath, _to: &VPath, _flags: RenameFlags) -> Result<()> {
        Err(not_found(from))
    }

    async fn set_meta(&self, p: &VPath, _m: &MetaPatch) -> Result<()> {
        Err(not_found(p))
    }

    fn watch(&self, p: &VPath) -> Result<BoxStream<'_, ChangeEvent>> {
        Err(Box::new(
            VfsError::new(
                ErrorKind::Fatal,
                "NullFs does not support watching (Caps::WATCH absent)",
            )
            .with_path(p.clone()),
        ))
    }

    async fn server_side_copy(&self, _from: &VPath, _to: &VPath) -> Result<CopyOutcome> {
        Ok(CopyOutcome::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use duet_types::{ErrorKind, UnixPathBuf};
    use futures_util::StreamExt;

    use super::*;

    fn p() -> VPath {
        VPath::local(UnixPathBuf::new("/anything").unwrap())
    }

    /// Like `Result::unwrap_err`, but doesn't require `T: Debug` — several
    /// `FileSystem` methods return `Ok` payloads (`Box<dyn AsyncWriteCommit>`,
    /// a boxed `Stream`) that deliberately aren't `Debug`.
    fn expect_err<T>(r: Result<T>) -> Box<VfsError> {
        match r {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(e) => e,
        }
    }

    #[test]
    fn constructs_as_boxed_and_arced_trait_object() {
        // T-2.2.2's object-safety requirement: the mount table is
        // `MountId -> Arc<dyn FileSystem>` (design.md §9.1), so this must
        // compile.
        let _boxed: Box<dyn FileSystem> = Box::new(NullFs);
        let _arced: Arc<dyn FileSystem> = Arc::new(NullFs);
    }

    #[tokio::test]
    async fn stat_reports_not_found() {
        let fs: Arc<dyn FileSystem> = Arc::new(NullFs);
        let err = fs.stat(&p(), false).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.path(), Some(&p()));
    }

    #[tokio::test]
    async fn volume_stats_reports_not_found() {
        let fs: Arc<dyn FileSystem> = Arc::new(NullFs);
        let err = fs.volume_stats(&p()).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn open_write_reports_fatal() {
        let fs: Arc<dyn FileSystem> = Arc::new(NullFs);
        let err = expect_err(fs.open_write(&p(), WriteOpts::overwrite()).await);
        assert_eq!(err.kind(), ErrorKind::Fatal);
    }

    #[tokio::test]
    async fn read_dir_yields_nothing() {
        let fs: Arc<dyn FileSystem> = Arc::new(NullFs);
        let mut stream = fs.read_dir(&p(), ListOpts::names_only());
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn server_side_copy_reports_unsupported_not_an_error() {
        let fs: Arc<dyn FileSystem> = Arc::new(NullFs);
        let outcome = fs.server_side_copy(&p(), &p()).await.unwrap();
        assert_eq!(outcome, CopyOutcome::Unsupported);
    }

    #[test]
    fn watch_reports_fatal_immediately() {
        let fs = NullFs;
        let err = expect_err(fs.watch(&p()));
        assert_eq!(err.kind(), ErrorKind::Fatal);
    }

    #[test]
    fn caps_are_empty() {
        assert_eq!(NullFs.caps(), Caps::empty());
        assert_eq!(NullFs.scheme(), "null");
    }
}
