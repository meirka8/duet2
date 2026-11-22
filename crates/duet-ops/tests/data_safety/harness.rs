// SPDX-License-Identifier: MIT
//! The crash harness: a throwaway world to run a job in, a child process
//! to run it and die in, and the same recovery path the product uses.
//!
//! # How a kill is injected
//!
//! The parent test builds a fixture and a `Plan`, writes both into a
//! [`ChildSpec`], and re-executes this very test binary with
//! [`CHILD_ENV`] pointing at the spec and `DUET_FAILPOINT` naming a crash
//! point. The child ([`child_main`], reached through the
//! `child_process_entry` test) runs the plan through the real
//! [`duet_ops::execute`] against the real [`LocalFs`] and, on reaching the
//! crash point, sends itself `SIGKILL` (`duet_vfs::failpoint`). The parent
//! then inspects what the kernel kept: the tree, and the journal.
//!
//! Nothing in the child is simulated. The kill is a real signal, the files
//! are real files, and no destructor, flush or `Drop` runs in between.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use duet_ops::{
    ExecutionControl, JobId, JobKind, JobReport, Journal, JournalReader, Plan, RecoveryReport,
    ResumePlan, discard_orphaned_partials, execute, prepare_resume,
};
use duet_types::{Caps, MetaPatch, Metadata, Result as VfsResult, UnixPathBuf, VPath};
use duet_vfs::{
    AsyncReadSeek, AsyncWriteCommit, ChangeEvent, CopyOutcome, DirEntry, FileSystem, ListOpts,
    LocalFs, Mode, RemoveKind, RenameFlags, VolumeStats, WriteOpts,
};
use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use crate::model::{StepStatus, Tree, read_journal, step_statuses};

pub const CHILD_ENV: &str = "DUET_DATA_SAFETY_CHILD";
pub const FAILPOINT_ENV: &str = "DUET_FAILPOINT";

/// The job id every interrupted job in the suite runs under; resumes get
/// [`RESUME_JOB`].
pub const JOB: u64 = 1;
pub const RESUME_JOB: u64 = 2;

pub fn vp(path: &Path) -> VPath {
    VPath::local(UnixPathBuf::new(path.to_str().expect("utf-8 test path")).expect("absolute"))
}

/// Deterministic, incompressible-looking bytes: the same `(seed, len)`
/// always yields the same content, and different seeds never collide, so
/// "which file's bytes are these" always has an answer.
pub fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

pub fn write_file(path: &Path, content: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// Pins an entry's mtime to a fixed instant, so that "the copy carries the
/// source's mtime" is a comparison against a known value and not against
/// whenever the fixture happened to be built.
pub fn set_mtime(path: &Path, secs: u64) {
    let file = std::fs::File::open(path).unwrap();
    file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
}

/// A temporary directory guaranteed to be on a different `st_dev` than
/// `other`, for cross-device moves. Fails loudly rather than skipping: a
/// data-safety run that quietly drops its cross-device half has not run.
pub fn tempdir_on_another_device(other: &Path) -> TempDir {
    use std::os::unix::fs::MetadataExt;
    let other_dev = std::fs::metadata(other).unwrap().dev();
    let candidates = [
        PathBuf::from("/dev/shm"),
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")),
        PathBuf::from("/var/tmp"),
        std::env::temp_dir(),
    ];
    for candidate in &candidates {
        let Ok(meta) = std::fs::metadata(candidate) else {
            continue;
        };
        if meta.dev() == other_dev {
            continue;
        }
        if let Ok(dir) = tempfile::Builder::new()
            .prefix("duet-data-safety-")
            .tempdir_in(candidate)
        {
            return dir;
        }
    }
    panic!(
        "no writable directory on a filesystem other than {}'s was found among {candidates:?}; \
         the cross-device data-safety cases cannot run on this machine",
        other.display()
    );
}

/// A scratch world: `root` is where the job's files live (and what gets
/// snapshotted), `state` holds the journal, and `far` -- when asked for --
/// is a second root on another device.
pub struct World {
    _tmp: TempDir,
    _far_tmp: Option<TempDir>,
    pub root: PathBuf,
    pub far: Option<PathBuf>,
    pub state: PathBuf,
}

impl World {
    pub fn new() -> World {
        let tmp = tempfile::Builder::new()
            .prefix("duet-data-safety-")
            .tempdir()
            .unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("world");
        let state = base.join("state");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        World {
            _tmp: tmp,
            _far_tmp: None,
            root,
            far: None,
            state,
        }
    }

    pub fn with_second_device() -> World {
        let mut world = World::new();
        let far_tmp = tempdir_on_another_device(&world.root);
        let far = far_tmp.path().canonicalize().unwrap().join("world");
        std::fs::create_dir_all(&far).unwrap();
        world.far = Some(far);
        world._far_tmp = Some(far_tmp);
        world
    }

    pub fn far(&self) -> &Path {
        self.far.as_deref().expect("world has no second device")
    }

    pub fn roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.root.clone()];
        roots.extend(self.far.clone());
        roots
    }

    pub fn capture(&self) -> Tree {
        Tree::capture(&self.roots())
    }

    pub fn statuses(&self, plan: &Plan, job_id: u64) -> Vec<StepStatus> {
        step_statuses(plan, &read_journal(&self.state, job_id))
    }

    /// The product's own recovery scan, narrowed to one job.
    pub fn scan(&self, job_id: u64) -> Option<RecoveryReport> {
        JournalReader::scan(&self.state)
            .expect("a journal left by a SIGKILL must always scan")
            .into_iter()
            .find(|report| report.job_id == JobId(job_id))
    }
}

/// Which `FileSystem` the child runs the plan against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    /// The real `LocalFs`: reflink / `copy_file_range` / sparse buffered.
    Local,
    /// `LocalFs` with `server_side_copy` reporting `Unsupported`, forcing
    /// the executor's own buffered `open_read`/`open_write` fallback -- the
    /// path every non-accelerating backend (archives, remotes) will take.
    Buffered,
}

impl Backend {
    pub fn fs(self) -> Arc<dyn FileSystem> {
        match self {
            Backend::Local => Arc::new(LocalFs),
            Backend::Buffered => Arc::new(BufferedOnly),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildSpec {
    pub plan: Plan,
    pub kind: JobKind,
    pub job_id: u64,
    pub state_dir: PathBuf,
    pub concurrency: usize,
    pub backend: Backend,
    /// Park instead of exiting once the job is done, so a parent that kills
    /// at an arbitrary instant always finds a process to kill.
    pub linger: bool,
}

/// How a child run ended.
#[derive(Debug)]
pub enum ChildEnd {
    /// Died of `SIGKILL` -- its own (a crash point) or the parent's.
    Killed,
    /// Ran the job to the end and reported.
    Finished(JobReport),
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

/// Runs a future on a fresh multi-thread runtime -- the shape `execute`
/// needs (it uses `block_in_place`).
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    runtime().block_on(future)
}

/// Runs `plan` to completion in this process.
pub fn run_job(
    fs: Arc<dyn FileSystem>,
    state_dir: &Path,
    job_id: u64,
    kind: JobKind,
    plan: Plan,
    concurrency: usize,
) -> JobReport {
    let journal = Journal::open(JobId(job_id), state_dir).expect("open journal");
    block_on(async move {
        let (events, _keep) = tokio::sync::mpsc::unbounded_channel();
        execute(
            fs,
            JobId(job_id),
            kind,
            plan,
            journal,
            concurrency,
            events,
            ExecutionControl::new(),
            None,
        )
        .await
    })
}

/// The child half. A no-op in an ordinary test run (no [`CHILD_ENV`]);
/// otherwise never returns.
pub fn child_main() {
    let Some(spec_path) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let spec_path = PathBuf::from(spec_path);
    let spec: ChildSpec =
        serde_json::from_slice(&std::fs::read(&spec_path).expect("read child spec"))
            .expect("parse child spec");
    let report = run_job(
        spec.backend.fs(),
        &spec.state_dir,
        spec.job_id,
        spec.kind,
        spec.plan,
        spec.concurrency,
    );
    std::fs::write(
        spec_path.with_extension("report"),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();
    if spec.linger {
        loop {
            std::thread::park();
        }
    }
    std::process::exit(0);
}

/// A spawned child and where it writes.
pub struct Child {
    process: std::process::Child,
    spec_path: PathBuf,
}

/// How long any single child may take before the harness declares it hung.
/// Generous: the slowest legitimate child copies a few tens of MiB.
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);

impl Child {
    pub fn spawn(world: &World, spec: &ChildSpec, failpoint: Option<&str>) -> Child {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let spec_path = world.state.join(format!("child-{n}.spec"));
        std::fs::write(&spec_path, serde_json::to_vec(spec).unwrap()).unwrap();
        let stderr = std::fs::File::create(spec_path.with_extension("stderr")).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "child_process_entry",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, &spec_path)
            .env_remove(FAILPOINT_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        if let Some(failpoint) = failpoint {
            command.env(FAILPOINT_ENV, failpoint);
        }
        Child {
            process: command.spawn().expect("spawn the crash-test child"),
            spec_path,
        }
    }

    pub fn kill(&mut self) {
        self.process.kill().expect("SIGKILL the crash-test child");
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(self.spec_path.with_extension("stderr")).unwrap_or_default()
    }

    /// Waits for the child to end and classifies how. Anything other than
    /// "killed by SIGKILL" or "exited 0 with a report" is a harness
    /// failure and panics with the child's stderr.
    pub fn wait(mut self) -> ChildEnd {
        use std::os::unix::process::ExitStatusExt;
        let deadline = Instant::now() + CHILD_TIMEOUT;
        let status: ExitStatus = loop {
            if let Some(status) = self.process.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = self.process.kill();
                let _ = self.process.wait();
                panic!(
                    "crash-test child still running after {CHILD_TIMEOUT:?}\n{}",
                    self.stderr()
                );
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        if status.signal() == Some(9) {
            return ChildEnd::Killed;
        }
        if status.success() {
            let report = std::fs::read(self.spec_path.with_extension("report"))
                .expect("a child that exits 0 writes its report first");
            return ChildEnd::Finished(serde_json::from_slice(&report).unwrap());
        }
        panic!(
            "crash-test child ended with {status:?} (neither SIGKILL nor a clean exit)\n{}",
            self.stderr()
        );
    }
}

/// Runs `spec` in a child told to die at `failpoint`, and insists that it
/// did. A child that runs to the end means the crash point was never
/// reached -- the test would be asserting about a crash that did not
/// happen, so that is a failure, not a pass.
pub fn crash_at(world: &World, spec: &ChildSpec, failpoint: &str) {
    match Child::spawn(world, spec, Some(failpoint)).wait() {
        ChildEnd::Killed => {}
        ChildEnd::Finished(report) => panic!(
            "crash point {failpoint:?} was never reached: the job ran to completion ({report:?})"
        ),
    }
}

/// Runs `spec` in a child with no crash point; it must finish.
pub fn run_to_end(world: &World, spec: &ChildSpec) -> JobReport {
    match Child::spawn(world, spec, None).wait() {
        ChildEnd::Finished(report) => report,
        ChildEnd::Killed => panic!("a child with no crash point was killed"),
    }
}

/// What a resume did, for assertions.
pub struct Resumed {
    pub decision: ResumePlan,
    /// `None` when nothing was left to run.
    pub report: Option<JobReport>,
}

/// The product's resume, step for step (`duet-ui`'s recovery dialog does
/// exactly this through the queue): [`prepare_resume`], run what is left
/// as a fresh job, then close the interrupted job's journal.
pub fn resume(world: &World, backend: Backend, report: &RecoveryReport) -> Resumed {
    resume_in(&world.state, backend, report)
}

/// [`resume`], for a journal that lives somewhere other than a `World`'s
/// own state directory.
pub fn resume_in(state_dir: &Path, backend: Backend, report: &RecoveryReport) -> Resumed {
    let fs = backend.fs();
    let decision = block_on(prepare_resume(&*fs, report)).expect("prepare_resume");
    let job_report = if decision.plan.steps.is_empty() {
        None
    } else {
        Some(run_job(
            fs,
            state_dir,
            RESUME_JOB,
            report.kind,
            decision.plan.clone(),
            4,
        ))
    };
    Journal::open(report.job_id, state_dir)
        .and_then(|journal| journal.resolve(report, "resumed by the data-safety suite"))
        .expect("resolve the interrupted job's journal");
    Resumed {
        decision,
        report: job_report,
    }
}

/// The product's discard: remove the scratch files, close the journal.
pub fn discard(world: &World, backend: Backend, report: &RecoveryReport) {
    let fs = backend.fs();
    block_on(discard_orphaned_partials(&*fs, report)).expect("discard_orphaned_partials");
    Journal::open(report.job_id, &world.state)
        .and_then(|journal| journal.resolve(report, "discarded by the data-safety suite"))
        .expect("resolve the interrupted job's journal");
}

/// After a resume or a discard, no journal may still ask for attention:
/// nothing dangling, nothing orphaned.
pub fn assert_nothing_left_to_recover(world: &World) {
    for report in JournalReader::scan(&world.state).unwrap() {
        assert!(
            report.incomplete_steps.is_empty() && report.orphaned_partials.is_empty(),
            "job {:?} still needs recovery after it was recovered: incomplete {:?}, partials {:?}",
            report.job_id,
            report.incomplete_steps,
            report.orphaned_partials
        );
    }
}

/// `LocalFs` that never accelerates a copy.
pub struct BufferedOnly;

#[async_trait]
impl FileSystem for BufferedOnly {
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
        LocalFs.stat(p, follow).await
    }
    async fn volume_stats(&self, p: &VPath) -> VfsResult<VolumeStats> {
        LocalFs.volume_stats(p).await
    }
    async fn open_read(&self, p: &VPath) -> VfsResult<Box<dyn AsyncReadSeek>> {
        LocalFs.open_read(p).await
    }
    async fn open_write(&self, p: &VPath, o: WriteOpts) -> VfsResult<Box<dyn AsyncWriteCommit>> {
        LocalFs.open_write(p, o).await
    }
    async fn create_dir(&self, p: &VPath, mode: Option<Mode>) -> VfsResult<()> {
        LocalFs.create_dir(p, mode).await
    }
    async fn remove(&self, p: &VPath, kind: RemoveKind) -> VfsResult<()> {
        LocalFs.remove(p, kind).await
    }
    async fn rename(&self, from: &VPath, to: &VPath, flags: RenameFlags) -> VfsResult<()> {
        LocalFs.rename(from, to, flags).await
    }
    async fn link(&self, source: &VPath, dest: &VPath) -> VfsResult<()> {
        LocalFs.link(source, dest).await
    }
    async fn symlink(&self, target: &str, link_path: &VPath) -> VfsResult<()> {
        LocalFs.symlink(target, link_path).await
    }
    async fn set_meta(&self, p: &VPath, m: &MetaPatch) -> VfsResult<()> {
        LocalFs.set_meta(p, m).await
    }
    fn watch(&self, p: &VPath) -> VfsResult<BoxStream<'_, ChangeEvent>> {
        LocalFs.watch(p)
    }
    async fn server_side_copy(
        &self,
        _from: &VPath,
        _to: &VPath,
        _on_progress: &(dyn Fn(u64) -> bool + Send + Sync),
    ) -> VfsResult<CopyOutcome> {
        Ok(CopyOutcome::Unsupported)
    }
    async fn sync(&self, p: &VPath) -> VfsResult<()> {
        LocalFs.sync(p).await
    }
}
