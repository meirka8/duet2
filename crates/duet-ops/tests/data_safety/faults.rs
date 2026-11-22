// SPDX-License-Identifier: MIT
//! I/O errors and mid-transfer disconnects, at every backend call.
//!
//! [`FaultFs`] wraps the real `LocalFs` and numbers every call the
//! executor makes through the `FileSystem` trait -- each `stat`, `rename`,
//! `set_meta`, `sync`, each chunk read from or written to a stream, each
//! commit. A job is first run clean to learn how many calls it makes, and
//! then run again once per call with a fault injected at exactly that
//! call:
//!
//! - **one I/O error** (`EIO`): that call fails, everything after works;
//! - **the device goes away**: that call and every later one fail, as
//!   after a pulled USB drive or a dropped mount, until it is plugged
//!   back in and the failed part of the job is re-run;
//! - **a transient disconnect**: a short burst of retryable failures, as
//!   from a network blip, after which the job must simply complete.
//!
//! A copy that fails "mid-transfer" is the adversarial version: the
//! backend has already written part of the file where it was told to
//! before it reports the error, and leaves that debris behind.
//!
//! This tier drives the executor through its public seam with injected
//! results. `privileged::` drives the same cases with errors that come out
//! of the kernel (`dm-flakey`, a detached device).

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};

use async_trait::async_trait;
use duet_ops::{JobReport, Plan, plan_from_report};
use duet_types::{Caps, MetaPatch, Metadata, Result as VfsResult, VPath, VfsError};
use duet_vfs::{
    AsyncReadSeek, AsyncWriteCommit, ChangeEvent, CommitOutcome, CopyOutcome, DirEntry, FileSystem,
    ListOpts, LocalFs, Mode, RemoveKind, RenameFlags, VolumeStats, WriteOpts,
};
use futures_util::stream::BoxStream;
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};

use crate::harness::{JOB, RESUME_JOB, World, run_job};
use crate::model::{Tree, check_finished, check_interrupted, check_no_content_lost, is_partial};
use crate::scenarios::{self, Scenario};

/// What goes wrong, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    None,
    /// Call number `at` fails once with `EIO`.
    ErrorAt {
        at: u64,
    },
    /// Call number `from` and every call after it fail (`EIO`), until
    /// [`FaultFs::reconnect`].
    GoneFrom {
        from: u64,
    },
    /// Calls `from .. from + len` fail with a retryable error
    /// (`ECONNRESET`), then service resumes by itself.
    BlipAt {
        from: u64,
        len: u64,
    },
}

struct Shared {
    calls: AtomicU64,
    fault: Fault,
    reconnected: AtomicBool,
}

impl Shared {
    /// Numbers this call and decides its fate.
    fn gate(&self) -> Result<(), std::io::Error> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let (fails, errno) = match self.fault {
            Fault::None => (false, 0),
            Fault::ErrorAt { at } => (n == at, 5), // EIO
            Fault::GoneFrom { from } => (n >= from && !self.reconnected.load(Ordering::SeqCst), 5),
            Fault::BlipAt { from, len } => (n >= from && n < from + len, 104), // ECONNRESET
        };
        if fails {
            Err(std::io::Error::from_raw_os_error(errno))
        } else {
            Ok(())
        }
    }

    fn vfs_gate(&self, path: &VPath) -> VfsResult<()> {
        self.gate()
            .map_err(|e| Box::new(VfsError::from_io(e).with_path(path.clone())))
    }
}

/// `LocalFs` with faults. `buffered` additionally refuses accelerated
/// copies, so the executor streams every file through `open_read` /
/// `open_write` -- which is where a transfer can fail between two chunks.
pub struct FaultFs {
    shared: Arc<Shared>,
    buffered: bool,
}

impl FaultFs {
    pub fn new(fault: Fault, buffered: bool) -> Arc<FaultFs> {
        Arc::new(FaultFs {
            shared: Arc::new(Shared {
                calls: AtomicU64::new(0),
                fault,
                reconnected: AtomicBool::new(false),
            }),
            buffered,
        })
    }

    pub fn calls(&self) -> u64 {
        self.shared.calls.load(Ordering::SeqCst)
    }

    /// The device is back.
    pub fn reconnect(&self) {
        self.shared.reconnected.store(true, Ordering::SeqCst);
    }
}

struct FaultyReader {
    inner: Box<dyn AsyncReadSeek>,
    shared: Arc<Shared>,
}

impl AsyncRead for FaultyReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Err(e) = self.shared.gate() {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut *self.inner).poll_read(cx, buf)
    }
}

impl AsyncSeek for FaultyReader {
    fn start_seek(mut self: Pin<&mut Self>, position: std::io::SeekFrom) -> std::io::Result<()> {
        Pin::new(&mut *self.inner).start_seek(position)
    }
    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
        Pin::new(&mut *self.inner).poll_complete(cx)
    }
}

struct FaultyWriter {
    inner: Box<dyn AsyncWriteCommit>,
    shared: Arc<Shared>,
    path: VPath,
}

impl AsyncWrite for FaultyWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if let Err(e) = self.shared.gate() {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut *self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.inner).poll_shutdown(cx)
    }
}

#[async_trait]
impl AsyncWriteCommit for FaultyWriter {
    async fn commit(self: Box<Self>) -> VfsResult<CommitOutcome> {
        let this = *self;
        if let Err(e) = this.shared.vfs_gate(&this.path) {
            // A commit that fails has published nothing; like the real
            // backend's own failed publish, it does not leave its staging
            // file behind when it can help it.
            let _ = this.inner.abort().await;
            return Err(e);
        }
        this.inner.commit().await
    }
    async fn abort(self: Box<Self>) -> VfsResult<()> {
        let this = *self;
        // Cleanup is a call like any other: on a device that is gone it
        // fails too, and the staging file stays where it is.
        this.shared.vfs_gate(&this.path)?;
        this.inner.abort().await
    }
}

#[async_trait]
impl FileSystem for FaultFs {
    fn scheme(&self) -> &'static str {
        LocalFs.scheme()
    }
    fn caps(&self) -> Caps {
        LocalFs.caps()
    }
    fn read_dir(&self, p: &VPath, opts: ListOpts) -> BoxStream<'_, VfsResult<Vec<DirEntry>>> {
        LocalFs.read_dir(p, opts)
    }
    async fn stat(&self, p: &VPath, follow: bool) -> VfsResult<Metadata> {
        self.shared.vfs_gate(p)?;
        LocalFs.stat(p, follow).await
    }
    async fn volume_stats(&self, p: &VPath) -> VfsResult<VolumeStats> {
        LocalFs.volume_stats(p).await
    }
    async fn open_read(&self, p: &VPath) -> VfsResult<Box<dyn AsyncReadSeek>> {
        self.shared.vfs_gate(p)?;
        Ok(Box::new(FaultyReader {
            inner: LocalFs.open_read(p).await?,
            shared: Arc::clone(&self.shared),
        }))
    }
    async fn open_write(&self, p: &VPath, o: WriteOpts) -> VfsResult<Box<dyn AsyncWriteCommit>> {
        self.shared.vfs_gate(p)?;
        Ok(Box::new(FaultyWriter {
            inner: LocalFs.open_write(p, o).await?,
            shared: Arc::clone(&self.shared),
            path: p.clone(),
        }))
    }
    async fn create_dir(&self, p: &VPath, mode: Option<Mode>) -> VfsResult<()> {
        self.shared.vfs_gate(p)?;
        LocalFs.create_dir(p, mode).await
    }
    async fn remove(&self, p: &VPath, kind: RemoveKind) -> VfsResult<()> {
        self.shared.vfs_gate(p)?;
        LocalFs.remove(p, kind).await
    }
    async fn rename(&self, from: &VPath, to: &VPath, flags: RenameFlags) -> VfsResult<()> {
        self.shared.vfs_gate(from)?;
        LocalFs.rename(from, to, flags).await
    }
    async fn link(&self, source: &VPath, dest: &VPath) -> VfsResult<()> {
        self.shared.vfs_gate(dest)?;
        LocalFs.link(source, dest).await
    }
    async fn symlink(&self, target: &str, link_path: &VPath) -> VfsResult<()> {
        self.shared.vfs_gate(link_path)?;
        LocalFs.symlink(target, link_path).await
    }
    async fn set_meta(&self, p: &VPath, m: &MetaPatch) -> VfsResult<()> {
        self.shared.vfs_gate(p)?;
        LocalFs.set_meta(p, m).await
    }
    fn watch(&self, p: &VPath) -> VfsResult<BoxStream<'_, ChangeEvent>> {
        LocalFs.watch(p)
    }
    async fn server_side_copy(
        &self,
        from: &VPath,
        to: &VPath,
        on_progress: &(dyn Fn(u64) -> bool + Send + Sync),
    ) -> VfsResult<CopyOutcome> {
        if self.buffered {
            return Ok(CopyOutcome::Unsupported);
        }
        // Two numbered moments: before the transfer starts, and partway
        // through it.
        self.shared.vfs_gate(to)?;
        if let Err(e) = self.shared.vfs_gate(to) {
            // Mid-transfer: the backend has already written half the file
            // at the path it was given, and now reports the error without
            // tidying up -- a device that died has no chance to.
            if LocalFs.server_side_copy(from, to, &|_| false).await.is_ok() {
                let path = crate::model::real(to);
                let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                file.set_len(len / 2).unwrap();
            }
            return Err(e);
        }
        LocalFs.server_side_copy(from, to, on_progress).await
    }
    async fn sync(&self, p: &VPath) -> VfsResult<()> {
        self.shared.vfs_gate(p)?;
        LocalFs.sync(p).await
    }
}

/// Runs `scenario` once against a `FaultFs`. Concurrency 1, so that "call
/// number k" names the same call on every run.
fn run_with(world: &World, scenario: &Scenario, fs: Arc<FaultFs>, job_id: u64) -> JobReport {
    run_job(
        fs,
        &world.state,
        job_id,
        scenario.kind,
        scenario.plan.clone(),
        1,
    )
}

/// How many backend calls a clean run of the scenario makes.
fn count_calls(
    new_world: fn() -> World,
    setup: &dyn Fn(&World) -> Scenario,
    buffered: bool,
) -> u64 {
    let world = new_world();
    let scenario = setup(&world);
    let before = world.capture();
    let fs = FaultFs::new(Fault::None, buffered);
    let report = run_with(&world, &scenario, Arc::clone(&fs), JOB);
    assert!(
        report.errors.is_empty() && report.skipped.is_empty(),
        "the fixture must run cleanly with no fault: {report:?}"
    );
    check_finished(&before, &scenario.plan, &world.capture()).expect("clean run");
    fs.calls()
}

/// The invariant after a job that *finished* (it was not killed) with
/// some steps failed: every step the journal says succeeded is in effect,
/// every other step is in its before- or after-state, any leftover is a
/// marked partial holding a prefix of its source, and no content is gone.
#[track_caller]
fn check_after_fault(
    world: &World,
    scenario: &Scenario,
    before: &Tree,
    plan: &Plan,
    job_id: u64,
    context: &str,
) -> Tree {
    let after = world.capture();
    let statuses = world.statuses(plan, job_id);
    if let Err(why) = check_interrupted(before, plan, &statuses, &after) {
        panic!("{context}: illegal state after the fault:\n{why}\njournal: {statuses:?}");
    }
    if let Err(why) = check_no_content_lost(before, &after, &scenario.deletable) {
        panic!("{context}: {why}");
    }
    after
}

/// A job that reports no error and no skip claims to be done -- so it
/// must be: complete, with nothing left behind.
#[track_caller]
fn assert_clean_report_means_finished(
    report: &JobReport,
    before: &Tree,
    plan: &Plan,
    after: &Tree,
    context: &str,
) -> bool {
    if !report.errors.is_empty() || !report.skipped.is_empty() {
        return false;
    }
    if let Err(why) = check_finished(before, plan, after) {
        panic!("{context}: the job reported success but is not complete:\n{why}");
    }
    true
}

fn sweep_calls(calls: u64, each: impl Fn(u64) + Sync) {
    let next = AtomicU64::new(0);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                loop {
                    let k = next.fetch_add(1, Ordering::SeqCst);
                    if k >= calls {
                        break;
                    }
                    each(k);
                }
            });
        }
    });
}

/// One `EIO`, at every call in turn.
fn single_io_error_sweep(
    new_world: fn() -> World,
    setup: impl Fn(&World) -> Scenario + Sync,
    buffered: bool,
) {
    let calls = count_calls(new_world, &setup, buffered);
    assert!(calls > 0);
    sweep_calls(calls, |k| {
        let context = format!("EIO at backend call {k} of {calls}");
        let world = new_world();
        let scenario = setup(&world);
        let before = world.capture();
        let fs = FaultFs::new(Fault::ErrorAt { at: k }, buffered);
        let report = run_with(&world, &scenario, Arc::clone(&fs), JOB);
        let after = check_after_fault(&world, &scenario, &before, &scenario.plan, JOB, &context);
        // One failed call never strands a scratch file: every cleanup
        // after it works.
        assert!(
            after.partials().is_empty(),
            "{context}: scratch files left behind: {:?}",
            after.partials()
        );
        if assert_clean_report_means_finished(&report, &before, &scenario.plan, &after, &context) {
            return;
        }
        // The user's "re-run failed" on the report, on a healthy backend.
        let rerun = plan_from_report(&scenario.plan, &report);
        let retry = Scenario {
            plan: rerun.clone(),
            kind: scenario.kind,
            deletable: scenario.deletable.clone(),
        };
        let rerun_report = run_with(
            &world,
            &retry,
            FaultFs::new(Fault::None, buffered),
            RESUME_JOB,
        );
        let finished = world.capture();
        check_no_content_lost(&before, &finished, &scenario.deletable)
            .unwrap_or_else(|why| panic!("{context}: after re-run: {why}"));
        if rerun_report.errors.is_empty() && rerun_report.skipped.is_empty() {
            check_finished(&before, &scenario.plan, &finished).unwrap_or_else(|why| {
                panic!("{context}: the re-run reported success but the job is not complete:\n{why}")
            });
        }
    });
}

/// The device disappears at call k and stays gone until the job has
/// given up; then it comes back and the failed part is re-run.
fn device_gone_sweep(
    new_world: fn() -> World,
    setup: impl Fn(&World) -> Scenario + Sync,
    buffered: bool,
) {
    let calls = count_calls(new_world, &setup, buffered);
    sweep_calls(calls, |k| {
        let context = format!("device gone from backend call {k} of {calls}");
        let world = new_world();
        let scenario = setup(&world);
        let before = world.capture();
        let fs = FaultFs::new(Fault::GoneFrom { from: k }, buffered);
        let report = run_with(&world, &scenario, Arc::clone(&fs), JOB);
        assert!(
            !report.errors.is_empty(),
            "{context}: a job whose device vanished cannot report a clean finish: {report:?}"
        );
        // With the device still gone: nothing lost, nothing half-made
        // under a real name. Scratch files may remain -- their cleanup
        // failed along with everything else -- but only marked ones.
        check_after_fault(&world, &scenario, &before, &scenario.plan, JOB, &context);

        fs.reconnect();
        let rerun = plan_from_report(&scenario.plan, &report);
        let retry = Scenario {
            plan: rerun,
            kind: scenario.kind,
            deletable: scenario.deletable.clone(),
        };
        let rerun_report = run_with(&world, &retry, fs, RESUME_JOB);
        let finished = world.capture();
        check_no_content_lost(&before, &finished, &scenario.deletable)
            .unwrap_or_else(|why| panic!("{context}: after reconnect and re-run: {why}"));
        if rerun_report.errors.is_empty() && rerun_report.skipped.is_empty() {
            // Complete -- apart from scratch files the dead device kept,
            // which are marked and which no real name points at.
            let mut expected = before.clone();
            expected.apply_all(&scenario.plan);
            let mut without_debris = finished.clone();
            without_debris.nodes.retain(|path, _| !is_partial(path));
            crate::model::compare(&expected, &without_debris).unwrap_or_else(|why| {
                panic!("{context}: the re-run reported success but the job is not complete:\n{why}")
            });
        }
    });
}

/// A burst of retryable failures starting at call k: the job must ride it
/// out and finish as if nothing had happened.
fn transient_disconnect_sweep(
    new_world: fn() -> World,
    setup: impl Fn(&World) -> Scenario + Sync,
    buffered: bool,
) {
    let calls = count_calls(new_world, &setup, buffered);
    sweep_calls(calls, |k| {
        let context = format!("transient disconnect at backend call {k} of {calls}");
        let world = new_world();
        let scenario = setup(&world);
        let before = world.capture();
        let fs = FaultFs::new(Fault::BlipAt { from: k, len: 2 }, buffered);
        let report = run_with(&world, &scenario, fs, JOB);
        let after = check_after_fault(&world, &scenario, &before, &scenario.plan, JOB, &context);
        assert!(
            after.partials().is_empty(),
            "{context}: scratch files left behind: {:?}",
            after.partials()
        );
        assert_clean_report_means_finished(&report, &before, &scenario.plan, &after, &context);
    });
}

fn copy(world: &World) -> Scenario {
    scenarios::copy_tree(world, true)
}

fn cross_device_move(world: &World) -> Scenario {
    scenarios::move_tree_across_devices(world, true)
}

#[test]
fn copy_survives_an_io_error_at_every_backend_call() {
    single_io_error_sweep(World::new, copy, false);
}

#[test]
fn buffered_copy_survives_an_io_error_at_every_backend_call() {
    single_io_error_sweep(World::new, copy, true);
}

#[test]
fn cross_device_move_survives_an_io_error_at_every_backend_call() {
    single_io_error_sweep(World::with_second_device, cross_device_move, false);
}

#[test]
fn buffered_cross_device_move_survives_an_io_error_at_every_backend_call() {
    single_io_error_sweep(World::with_second_device, cross_device_move, true);
}

#[test]
fn same_device_move_survives_an_io_error_at_every_backend_call() {
    single_io_error_sweep(World::new, scenarios::move_tree_same_device, false);
}

#[test]
fn permanent_delete_survives_an_io_error_at_every_backend_call() {
    single_io_error_sweep(World::new, scenarios::delete_tree, false);
}

#[test]
fn delete_to_trash_survives_an_io_error_at_every_backend_call() {
    single_io_error_sweep(World::new, scenarios::trash_tree, false);
}

#[test]
fn copy_survives_the_device_disappearing_at_every_backend_call() {
    device_gone_sweep(World::new, copy, false);
}

#[test]
fn buffered_copy_survives_the_device_disappearing_mid_transfer() {
    device_gone_sweep(World::new, copy, true);
}

#[test]
fn cross_device_move_survives_the_device_disappearing_at_every_backend_call() {
    device_gone_sweep(World::with_second_device, cross_device_move, false);
}

#[test]
fn buffered_cross_device_move_survives_the_device_disappearing_mid_transfer() {
    device_gone_sweep(World::with_second_device, cross_device_move, true);
}

#[test]
fn delete_to_trash_survives_the_device_disappearing_at_every_backend_call() {
    device_gone_sweep(World::new, scenarios::trash_tree, false);
}

#[test]
fn copy_rides_out_a_transient_disconnect_at_every_backend_call() {
    transient_disconnect_sweep(World::new, copy, false);
}

#[test]
fn buffered_copy_rides_out_a_transient_disconnect_mid_transfer() {
    transient_disconnect_sweep(World::new, copy, true);
}

#[test]
fn cross_device_move_rides_out_a_transient_disconnect_at_every_backend_call() {
    transient_disconnect_sweep(World::with_second_device, cross_device_move, false);
}

#[test]
fn buffered_cross_device_move_rides_out_a_transient_disconnect_mid_transfer() {
    transient_disconnect_sweep(World::with_second_device, cross_device_move, true);
}
