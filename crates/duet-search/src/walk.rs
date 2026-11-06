// SPDX-License-Identifier: MIT
//! The traversal: a pool of worker threads draining one shared queue of
//! directories, matching every entry as it is listed, and streaming hits
//! to a sink as they are found.
//!
//! # Shape
//!
//! [`start`] spawns one coordinator thread, which spawns the workers
//! (scoped, so it joins them), sends a [`SearchEvent::Progress`] snapshot
//! every [`PROGRESS_INTERVAL`] while they run, and finally one
//! [`SearchEvent::Finished`]. Workers send [`SearchEvent::Hits`] batches
//! and (capped) [`SearchEvent::Error`]s straight to the sink. Plain
//! threads, not a Tokio or Rayon pool: the work is blocking `getdents`/
//! `read` from start to end, and a runtime-owned pool would keep a test's
//! runtime from shutting down while a search is still draining (the
//! viewer's worker documents the same trap).
//!
//! The queue is FIFO, so the tree is walked breadth-first: shallow hits
//! come first, and one huge, deep subtree cannot starve the rest.
//!
//! # Cost per entry
//!
//! An entry that does not match the name costs only its `getdents` record
//! (`d_type` gives the kind); `lstat` runs for name matches (their size,
//! date and mode go into the result) and for directories (their `(dev,
//! ino)` feeds loop detection and the mount checks). Content is only read
//! for regular files that passed every other test.
//!
//! # Mounts and loops
//!
//! Every directory's `(dev, ino)` goes into one visited set before it is
//! queued, so a symlink loop (with `follow_symlinks`), a bind mount of an
//! ancestor, or two overlapping roots never walk the same directory
//! twice. Crossing into another filesystem is refused outright with
//! `one_filesystem`; otherwise it is allowed except into kernel pseudo-
//! filesystems (/proc, /sys, cgroups, debugfs, ...), which hold no user
//! files and can hang a reader. Searching *from* such a mount still
//! works -- only crossing into one is skipped.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs::{self, File, Metadata as StdMetadata};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use duet_types::{EntryKind, Metadata, Timestamp};

use crate::content::FileVerdict;
use crate::spec::{CompiledSearch, KindFilter};

/// How often the coordinator reports progress while workers run.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// A worker flushes its pending hits at least this often...
const FLUSH_INTERVAL: Duration = Duration::from_millis(40);
/// ...or as soon as it has this many.
const FLUSH_COUNT: usize = 256;
/// Error events beyond this many are counted, not sent.
const MAX_ERROR_EVENTS: u64 = 100;

/// One found entry: its absolute path and the same metadata a panel
/// listing would hold for it (kind, size, mtime, mode -- see
/// [`metadata_from_std`]).
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: PathBuf,
    pub metadata: Metadata,
}

/// Counters, as of one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchProgress {
    pub dirs: u64,
    /// Entries examined (every name listed, matched or not).
    pub entries: u64,
    /// Content bytes read.
    pub bytes: u64,
    pub hits: u64,
    pub errors: u64,
    /// The directory a worker most recently started on.
    pub current: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum SearchEvent {
    Hits(Vec<SearchHit>),
    Progress(SearchProgress),
    /// A directory that could not be listed, or a file that could not be
    /// read. Only the first [`MAX_ERROR_EVENTS`] are sent.
    Error {
        path: PathBuf,
        message: String,
    },
    Finished {
        progress: SearchProgress,
        elapsed: Duration,
        cancelled: bool,
    },
}

/// Where events go. Called from several threads at once.
pub type SearchSink = Arc<dyn Fn(SearchEvent) + Send + Sync>;

/// A running search. Dropping it cancels the search (without waiting:
/// the workers notice within one directory entry or one read).
pub struct SearchHandle {
    cancel: Arc<AtomicBool>,
}

impl SearchHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

impl Drop for SearchHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Starts `search` on a background coordinator thread.
pub fn start(search: CompiledSearch, sink: SearchSink) -> SearchHandle {
    let cancel = Arc::new(AtomicBool::new(false));
    let thread_cancel = cancel.clone();
    let spawned = std::thread::Builder::new()
        .name("duet-search".into())
        .spawn(move || {
            run(search, sink, thread_cancel);
        });
    if let Err(err) = spawned {
        tracing::error!(target: "duet_search", "could not start the search thread: {err}");
    }
    SearchHandle { cancel }
}

/// Runs `search` on the calling thread (plus its workers) and returns
/// when it is finished -- what [`start`]'s thread does; tests and
/// benchmarks call it directly. The final [`SearchEvent::Finished`] goes
/// to `sink` as well as being returned.
pub fn run(search: CompiledSearch, sink: SearchSink, cancel: Arc<AtomicBool>) -> SearchProgress {
    let started = Instant::now();
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(2, 8);
    let shared = Shared {
        queue: Mutex::new(Queue::default()),
        work: Condvar::new(),
        finished: Condvar::new(),
        visited: Mutex::new(HashSet::new()),
        cancel,
        stats: Stats::default(),
        current: Mutex::new(None),
        search,
        sink,
    };
    shared.seed_roots();

    std::thread::scope(|scope| {
        for ix in 0..threads {
            let shared = &shared;
            let spawned = std::thread::Builder::new()
                .name(format!("duet-search-{ix}"))
                .spawn_scoped(scope, move || Worker::new(shared).run());
            if let Err(err) = spawned {
                tracing::error!(target: "duet_search", "could not start search worker {ix}: {err}");
            }
        }
        let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        while !queue.done {
            let (guard, timeout) = shared
                .finished
                .wait_timeout(queue, PROGRESS_INTERVAL)
                .unwrap_or_else(|e| e.into_inner());
            queue = guard;
            if !queue.done && timeout.timed_out() {
                drop(queue);
                (shared.sink)(SearchEvent::Progress(shared.snapshot()));
                queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            }
        }
    });

    let progress = shared.snapshot();
    (shared.sink)(SearchEvent::Finished {
        progress: progress.clone(),
        elapsed: started.elapsed(),
        cancelled: shared.cancel.load(Ordering::Relaxed),
    });
    progress
}

#[derive(Default)]
struct Queue {
    pending: VecDeque<DirJob>,
    /// Workers currently processing a directory (and so possibly about
    /// to queue more).
    active: usize,
    done: bool,
}

struct DirJob {
    path: PathBuf,
    depth: u32,
    dev: u64,
}

#[derive(Default)]
struct Stats {
    dirs: AtomicU64,
    entries: AtomicU64,
    bytes: AtomicU64,
    hits: AtomicU64,
    errors: AtomicU64,
}

struct Shared {
    queue: Mutex<Queue>,
    /// Signalled when work is queued or the walk is done.
    work: Condvar,
    /// Signalled when the walk is done (the coordinator waits on it).
    finished: Condvar,
    visited: Mutex<HashSet<(u64, u64)>>,
    cancel: Arc<AtomicBool>,
    stats: Stats,
    current: Mutex<Option<PathBuf>>,
    search: CompiledSearch,
    sink: SearchSink,
}

impl Shared {
    fn seed_roots(&self) {
        let mut pending = VecDeque::new();
        for root in &self.search.roots {
            // A root is followed even if it is a symlink: the user named it.
            match fs::metadata(root) {
                Ok(meta) if meta.is_dir() => {
                    if self.first_visit(&meta) {
                        pending.push_back(DirJob {
                            path: root.clone(),
                            depth: 0,
                            dev: meta.dev(),
                        });
                    }
                }
                Ok(_) => self.error(root, "not a directory".to_string()),
                Err(err) => self.error(root, err.to_string()),
            }
        }
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.pending = pending;
        queue.done = queue.pending.is_empty();
    }

    fn first_visit(&self, meta: &StdMetadata) -> bool {
        self.visited
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((meta.dev(), meta.ino()))
    }

    fn error(&self, path: &Path, message: String) {
        let n = self.stats.errors.fetch_add(1, Ordering::Relaxed);
        if n < MAX_ERROR_EVENTS {
            (self.sink)(SearchEvent::Error {
                path: path.to_path_buf(),
                message,
            });
        }
    }

    fn snapshot(&self) -> SearchProgress {
        SearchProgress {
            dirs: self.stats.dirs.load(Ordering::Relaxed),
            entries: self.stats.entries.load(Ordering::Relaxed),
            bytes: self.stats.bytes.load(Ordering::Relaxed),
            hits: self.stats.hits.load(Ordering::Relaxed),
            errors: self.stats.errors.load(Ordering::Relaxed),
            current: self
                .current
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// The next directory, or `None` once there is nothing left (or the
    /// search was cancelled).
    fn next_job(&self) -> Option<DirJob> {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if self.cancelled() {
                queue.pending.clear();
            }
            if let Some(job) = queue.pending.pop_front() {
                queue.active += 1;
                return Some(job);
            }
            if queue.active == 0 {
                queue.done = true;
                self.work.notify_all();
                self.finished.notify_all();
                return None;
            }
            queue = self.work.wait(queue).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Queues what one directory turned up and marks it finished.
    fn complete_job(&self, subdirs: Vec<DirJob>) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.active -= 1;
        if !self.cancelled() {
            queue.pending.extend(subdirs);
        }
        if queue.pending.is_empty() && queue.active == 0 {
            queue.done = true;
            self.finished.notify_all();
        }
        self.work.notify_all();
    }
}

struct Worker<'a> {
    shared: &'a Shared,
    hits: Vec<SearchHit>,
    last_flush: Instant,
    /// Content-search window buffer, reused for every file.
    buf: Vec<u8>,
}

impl<'a> Worker<'a> {
    fn new(shared: &'a Shared) -> Self {
        Self {
            shared,
            hits: Vec::new(),
            last_flush: Instant::now(),
            buf: Vec::new(),
        }
    }

    fn run(mut self) {
        while let Some(job) = self.shared.next_job() {
            let subdirs = self.process(&job);
            self.flush();
            self.shared.complete_job(subdirs);
        }
    }

    fn process(&mut self, job: &DirJob) -> Vec<DirJob> {
        let shared = self.shared;
        let search = &shared.search;
        *shared.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(job.path.clone());
        let entries = match fs::read_dir(&job.path) {
            Ok(entries) => entries,
            Err(err) => {
                shared.error(&job.path, err.to_string());
                return Vec::new();
            }
        };
        shared.stats.dirs.fetch_add(1, Ordering::Relaxed);
        let descend = search.max_depth.is_none_or(|max| job.depth < max);
        let mut subdirs = Vec::new();

        for entry in entries {
            if shared.cancelled() {
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    shared.error(&job.path, err.to_string());
                    continue;
                }
            };
            shared.stats.entries.fetch_add(1, Ordering::Relaxed);
            let name = entry.file_name();
            let name_bytes = name.as_bytes();
            if !search.include_hidden && name_bytes.first() == Some(&b'.') {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            // The target's metadata, when a symlink is being followed.
            let followed = if file_type.is_symlink() && search.follow_symlinks {
                fs::metadata(&path).ok()
            } else {
                None
            };
            let is_dir = match &followed {
                Some(meta) => meta.is_dir(),
                None => file_type.is_dir(),
            };
            if is_dir && search.name.prunes_dir(name_bytes) {
                continue;
            }

            if is_dir && descend {
                let meta = match &followed {
                    Some(meta) => Ok(meta.clone()),
                    None => fs::symlink_metadata(&path),
                };
                match meta {
                    Ok(meta) => {
                        if self.may_enter(&path, &meta, job.dev) {
                            subdirs.push(DirJob {
                                path: path.clone(),
                                depth: job.depth + 1,
                                dev: meta.dev(),
                            });
                        }
                    }
                    Err(err) => shared.error(&path, err.to_string()),
                }
            }

            let kind_ok = match search.kind {
                KindFilter::Any => true,
                KindFilter::FilesOnly => !is_dir,
                KindFilter::DirsOnly => is_dir,
            };
            if !kind_ok || !search.name.matches(name_bytes) {
                continue;
            }
            self.consider(path, followed);
        }
        subdirs
    }

    /// Whether the walk may enter the directory at `path`.
    fn may_enter(&self, path: &Path, meta: &StdMetadata, parent_dev: u64) -> bool {
        let search = &self.shared.search;
        if meta.dev() != parent_dev {
            if search.one_filesystem {
                return false;
            }
            if is_pseudo_filesystem(path) {
                return false;
            }
        }
        self.shared.first_visit(meta)
    }

    /// Applies the metadata and content criteria to a name match.
    fn consider(&mut self, path: PathBuf, followed: Option<StdMetadata>) {
        let shared = self.shared;
        let search = &shared.search;
        let own = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) => {
                // Vanished between listing and stat: not an error worth
                // reporting, just not a result.
                if err.kind() != std::io::ErrorKind::NotFound {
                    shared.error(&path, err.to_string());
                }
                return;
            }
        };
        // Filters judge what the entry *is* -- the target, when following.
        let judged = followed.as_ref().unwrap_or(&own);
        let (min, max) = search.size;
        if (min.is_some() || max.is_some())
            && (judged.is_dir()
                || min.is_some_and(|m| judged.len() < m)
                || max.is_some_and(|m| judged.len() > m))
        {
            return;
        }
        let (after, before) = search.modified;
        let mtime = judged.mtime();
        if after.is_some_and(|a| mtime < a) || before.is_some_and(|b| mtime > b) {
            return;
        }
        if let Some(content) = &search.content {
            if !judged.file_type().is_file() {
                return;
            }
            // O_NONBLOCK: a FIFO swapped in since the stat must not hang
            // the worker (it has no effect on a regular file).
            let file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
                .open(&path);
            let mut file: File = match file {
                Ok(file) => file,
                Err(err) => {
                    shared.error(&path, err.to_string());
                    return;
                }
            };
            match content.check(
                &mut file,
                &mut self.buf,
                &shared.stats.bytes,
                &shared.cancel,
            ) {
                Ok(FileVerdict::Match) => {}
                Ok(FileVerdict::NoMatch | FileVerdict::Skipped) => return,
                Err(err) => {
                    shared.error(&path, err.to_string());
                    return;
                }
            }
        }
        shared.stats.hits.fetch_add(1, Ordering::Relaxed);
        self.hits.push(SearchHit {
            metadata: metadata_from_std(&own),
            path,
        });
        if self.hits.len() >= FLUSH_COUNT || self.last_flush.elapsed() >= FLUSH_INTERVAL {
            self.flush();
        }
    }

    fn flush(&mut self) {
        self.last_flush = Instant::now();
        if !self.hits.is_empty() {
            (self.shared.sink)(SearchEvent::Hits(std::mem::take(&mut self.hits)));
        }
    }
}

/// The panel-listing subset of `meta` (kind, size, mtime, mode) -- what
/// `duet_vfs::LocalFs::read_dir` fills for a panel, so a fed result costs
/// the `EntryStore` no more than a listed one.
pub fn metadata_from_std(meta: &StdMetadata) -> Metadata {
    let ft = meta.file_type();
    let kind = if ft.is_dir() {
        EntryKind::Directory
    } else if ft.is_file() {
        EntryKind::File
    } else if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_fifo() {
        EntryKind::Fifo
    } else if ft.is_socket() {
        EntryKind::Socket
    } else if ft.is_block_device() {
        EntryKind::BlockDevice
    } else if ft.is_char_device() {
        EntryKind::CharDevice
    } else {
        EntryKind::Unknown
    };
    let mut out = Metadata::minimal(kind);
    out.size = meta.len();
    out.modified = Some(Timestamp {
        secs: meta.mtime(),
        nanos: meta.mtime_nsec().clamp(0, 999_999_999) as u32,
    });
    out.mode = Some(meta.mode());
    out
}

/// Whether `path` is on a kernel pseudo-filesystem the walk should not
/// cross into. Magic numbers from `linux/magic.h`.
fn is_pseudo_filesystem(path: &Path) -> bool {
    const PSEUDO: &[u64] = &[
        0x9fa0,      // proc
        0x6265_6572, // sysfs
        0x1cd1,      // devpts
        0x0027_e0eb, // cgroup
        0x6367_7270, // cgroup2
        0x6462_6720, // debugfs
        0x7472_6163, // tracefs
        0x7363_6673, // securityfs
        0x6165_676c, // pstore
        0xcafe_4a11, // bpf
        0x6265_6570, // configfs
        0x6573_5543, // fusectl
        0x4249_4e4d, // binfmt_misc
        0xde5e_81e4, // efivarfs
        0x1980_0202, // mqueue
        0x6e73_6673, // nsfs
    ];
    match rustix::fs::statfs(path) {
        Ok(stat) => PSEUDO.contains(&(stat.f_type as u64)),
        Err(_) => false,
    }
}

/// `path` relative to `root`, as a panel name -- or the whole path when
/// it is not under `root`. What "feed to panel" lists.
pub fn relative_name(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    String::from_utf8_lossy(OsStr::as_bytes(rel.as_os_str())).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{ContentEncodings, ContentSpec};
    use crate::spec::SearchSpec;
    use std::io::Write as _;

    /// Runs `spec` to completion, returning the hit paths relative to
    /// the first root, sorted, and the final progress.
    fn search(spec: SearchSpec) -> (Vec<String>, SearchProgress) {
        let root = spec.roots[0].clone();
        let compiled = spec.compile().unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let sink_hits = hits.clone();
        let sink: SearchSink = Arc::new(move |event| {
            if let SearchEvent::Hits(batch) = event {
                sink_hits.lock().unwrap().extend(batch);
            }
        });
        let progress = run(compiled, sink, Arc::new(AtomicBool::new(false)));
        let mut names: Vec<String> = hits
            .lock()
            .unwrap()
            .iter()
            .map(|h| relative_name(&root, &h.path))
            .collect();
        names.sort();
        (names, progress)
    }

    fn write(path: &Path, content: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        File::create(path).unwrap().write_all(content).unwrap();
    }

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        write(&p.join("a.rs"), b"fn main() { println!(\"hello\"); }\n");
        write(&p.join("b.txt"), b"nothing to see\n");
        write(&p.join("sub/c.rs"), b"// HELLO there\n");
        write(&p.join("sub/deep/d.rs"), b"no greeting\n");
        write(&p.join("sub/deep/big.bin"), &vec![b'x'; 10_000]);
        write(&p.join(".hidden/e.rs"), b"hello\n");
        write(&p.join(".dot.rs"), b"\n");
        write(&p.join("node_modules/f.rs"), b"hello\n");
        dir
    }

    fn spec_for(dir: &tempfile::TempDir) -> SearchSpec {
        SearchSpec {
            roots: vec![dir.path().to_path_buf()],
            ..SearchSpec::default()
        }
    }

    #[test]
    fn mask_search_walks_the_whole_tree() {
        let dir = tree();
        let (names, progress) = search(SearchSpec {
            name: "*.rs".into(),
            ..spec_for(&dir)
        });
        assert_eq!(
            names,
            [
                ".dot.rs",
                ".hidden/e.rs",
                "a.rs",
                "node_modules/f.rs",
                "sub/c.rs",
                "sub/deep/d.rs"
            ]
        );
        assert_eq!(progress.hits, 6);
        assert_eq!(progress.dirs, 5);
        assert_eq!(progress.errors, 0);
    }

    #[test]
    fn directories_match_names_too_unless_files_only() {
        let dir = tree();
        let (names, _) = search(SearchSpec {
            name: "deep".into(),
            ..spec_for(&dir)
        });
        assert_eq!(names, ["sub/deep"]);
        let (names, _) = search(SearchSpec {
            name: "deep".into(),
            kind: KindFilter::FilesOnly,
            ..spec_for(&dir)
        });
        assert!(names.is_empty());
        let (names, _) = search(SearchSpec {
            kind: KindFilter::DirsOnly,
            ..spec_for(&dir)
        });
        assert_eq!(names, [".hidden", "node_modules", "sub", "sub/deep"]);
    }

    #[test]
    fn hidden_entries_can_be_left_out() {
        let dir = tree();
        let (names, _) = search(SearchSpec {
            name: "*.rs".into(),
            include_hidden: false,
            ..spec_for(&dir)
        });
        assert_eq!(
            names,
            ["a.rs", "node_modules/f.rs", "sub/c.rs", "sub/deep/d.rs"]
        );
    }

    #[test]
    fn excluded_directories_are_not_entered() {
        let dir = tree();
        let (names, _) = search(SearchSpec {
            name: "*.rs | node_modules/ .hidden/".into(),
            ..spec_for(&dir)
        });
        assert_eq!(names, [".dot.rs", "a.rs", "sub/c.rs", "sub/deep/d.rs"]);
    }

    #[test]
    fn depth_limit() {
        let dir = tree();
        let (names, _) = search(SearchSpec {
            name: "*.rs".into(),
            max_depth: Some(0),
            ..spec_for(&dir)
        });
        assert_eq!(names, [".dot.rs", "a.rs"]);
        let (names, _) = search(SearchSpec {
            name: "*.rs".into(),
            max_depth: Some(1),
            ..spec_for(&dir)
        });
        assert_eq!(
            names,
            [
                ".dot.rs",
                ".hidden/e.rs",
                "a.rs",
                "node_modules/f.rs",
                "sub/c.rs"
            ]
        );
    }

    #[test]
    fn content_search_case_insensitive_by_default() {
        let dir = tree();
        let (names, progress) = search(SearchSpec {
            name: "*.rs".into(),
            content: Some(ContentSpec {
                text: "hello".into(),
                ..ContentSpec::default()
            }),
            include_hidden: false,
            ..spec_for(&dir)
        });
        assert_eq!(names, ["a.rs", "node_modules/f.rs", "sub/c.rs"]);
        assert!(progress.bytes > 0);
    }

    #[test]
    fn content_search_not_containing() {
        let dir = tree();
        let (names, _) = search(SearchSpec {
            name: "*.rs | .hidden/ node_modules/".into(),
            content: Some(ContentSpec {
                text: "hello".into(),
                not_containing: true,
                ..ContentSpec::default()
            }),
            ..spec_for(&dir)
        });
        assert_eq!(names, [".dot.rs", "sub/deep/d.rs"]);
    }

    #[test]
    fn content_search_never_reports_directories() {
        let dir = tree();
        let (names, _) = search(SearchSpec {
            content: Some(ContentSpec {
                text: "no greeting".into(),
                encodings: ContentEncodings::Utf8AndUtf16,
                ..ContentSpec::default()
            }),
            ..spec_for(&dir)
        });
        assert_eq!(names, ["sub/deep/d.rs"]);
    }

    #[test]
    fn size_filter_excludes_directories() {
        let dir = tree();
        let (names, _) = search(SearchSpec {
            size_min: Some(5_000),
            ..spec_for(&dir)
        });
        assert_eq!(names, ["sub/deep/big.bin"]);
        let (names, _) = search(SearchSpec {
            name: "*.rs".into(),
            size_max: Some(5),
            ..spec_for(&dir)
        });
        assert_eq!(names, [".dot.rs"]);
    }

    #[test]
    fn date_filter() {
        let dir = tree();
        let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        File::options()
            .write(true)
            .open(dir.path().join("b.txt"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let (names, _) = search(SearchSpec {
            modified_before: Some(1_000_000_100),
            ..spec_for(&dir)
        });
        assert_eq!(names, ["b.txt"]);
        let (names, _) = search(SearchSpec {
            name: "*.txt".into(),
            modified_after: Some(1_000_000_100),
            ..spec_for(&dir)
        });
        assert!(names.is_empty());
    }

    #[test]
    fn symlinks_are_listed_not_followed_by_default() {
        let dir = tree();
        std::os::unix::fs::symlink(dir.path().join("sub"), dir.path().join("link")).unwrap();
        let (names, _) = search(SearchSpec {
            name: "c.rs link".into(),
            ..spec_for(&dir)
        });
        assert_eq!(names, ["link", "sub/c.rs"]);
    }

    #[test]
    fn following_symlinks_survives_a_loop() {
        let dir = tree();
        // sub/deep/up -> sub: a loop once followed.
        std::os::unix::fs::symlink(dir.path().join("sub"), dir.path().join("sub/deep/up")).unwrap();
        let (names, progress) = search(SearchSpec {
            name: "c.rs".into(),
            follow_symlinks: true,
            ..spec_for(&dir)
        });
        // Found once: the walk never re-enters `sub` through the link.
        assert_eq!(names, ["sub/c.rs"]);
        assert_eq!(progress.dirs, 5);
    }

    #[test]
    fn overlapping_roots_walk_each_directory_once() {
        let dir = tree();
        let (names, _) = search(SearchSpec {
            roots: vec![dir.path().to_path_buf(), dir.path().join("sub")],
            name: "c.rs".into(),
            ..SearchSpec::default()
        });
        assert_eq!(names, ["sub/c.rs"]);
    }

    #[test]
    fn a_missing_root_is_an_error_not_a_hang() {
        let dir = tree();
        let (names, progress) = search(SearchSpec {
            roots: vec![dir.path().join("nope")],
            ..SearchSpec::default()
        });
        assert!(names.is_empty());
        assert_eq!(progress.errors, 1);
    }

    #[test]
    fn unreadable_directories_are_counted() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tree();
        let locked = dir.path().join("sub/deep");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let readable_anyway = fs::read_dir(&locked).is_ok(); // running as root
        let (names, progress) = search(SearchSpec {
            name: "*.rs".into(),
            include_hidden: false,
            ..spec_for(&dir)
        });
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        if !readable_anyway {
            assert_eq!(names, ["a.rs", "node_modules/f.rs", "sub/c.rs"]);
            assert_eq!(progress.errors, 1);
        }
    }

    #[test]
    fn cancelling_stops_the_walk_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..50 {
            write(&dir.path().join(format!("d{i}/f.txt")), b"x");
        }
        let compiled = SearchSpec {
            roots: vec![dir.path().to_path_buf()],
            ..SearchSpec::default()
        }
        .compile()
        .unwrap();
        let finished = Arc::new(Mutex::new(None));
        let sink_finished = finished.clone();
        let sink: SearchSink = Arc::new(move |event| {
            if let SearchEvent::Finished { cancelled, .. } = event {
                *sink_finished.lock().unwrap() = Some(cancelled);
            }
        });
        let progress = run(compiled, sink, Arc::new(AtomicBool::new(true)));
        assert_eq!(*finished.lock().unwrap(), Some(true));
        assert_eq!(progress.dirs, 0);
    }

    #[test]
    fn start_streams_to_the_sink_and_finishes() {
        let dir = tree();
        let compiled = SearchSpec {
            name: "*.rs".into(),
            ..spec_for(&dir)
        }
        .compile()
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        let sink: SearchSink = Arc::new(move |event| {
            let _ = tx.lock().unwrap().send(event);
        });
        let handle = start(compiled, sink);
        let mut hits = 0;
        loop {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                SearchEvent::Hits(batch) => hits += batch.len(),
                SearchEvent::Finished { cancelled, .. } => {
                    assert!(!cancelled);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(hits, 6);
        assert!(!handle.is_cancelled());
        drop(handle);
    }

    #[test]
    fn hit_metadata_is_the_panel_subset() {
        let dir = tree();
        let compiled = SearchSpec {
            name: "big.bin".into(),
            ..spec_for(&dir)
        }
        .compile()
        .unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let sink_hits = hits.clone();
        let sink: SearchSink = Arc::new(move |event| {
            if let SearchEvent::Hits(batch) = event {
                sink_hits.lock().unwrap().extend(batch);
            }
        });
        run(compiled, sink, Arc::new(AtomicBool::new(false)));
        let hits = hits.lock().unwrap();
        let meta = &hits[0].metadata;
        assert_eq!(meta.kind, EntryKind::File);
        assert_eq!(meta.size, 10_000);
        assert!(meta.modified.is_some() && meta.mode.is_some());
        assert!(meta.uid.is_none() && meta.ino.is_none());
    }

    #[test]
    fn relative_names() {
        assert_eq!(relative_name(Path::new("/a"), Path::new("/a/b/c")), "b/c");
        assert_eq!(relative_name(Path::new("/a"), Path::new("/x/y")), "/x/y");
    }
}
