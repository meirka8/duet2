// SPDX-License-Identifier: MIT
//! An independent model of what a `Plan` does to a directory tree, and the
//! invariant checker built on it.
//!
//! The suite never asks the engine what the right answer is. It captures
//! the tree before a job ([`Tree::capture`]), predicts from the plan alone
//! what each step does to it ([`Tree::apply`]), and then compares that
//! prediction with what is really on disk after an injected fault
//! ([`check_interrupted`]) and after recovery ([`check_finished`]).
//!
//! design.md §14.4 states the assertion in one sentence: *for every
//! injection point, either the source is intact and the destination is
//! absent/partial-and-marked, or the destination is complete. Never
//! anything else.* Here that becomes, path by path:
//!
//! - a path no in-flight step touches must be exactly what the durably
//!   completed prefix of the plan says it is;
//! - a path an in-flight step touches must be in its before-state or its
//!   after-state (plus the per-kind intermediate states
//!   `docs/crash-safety.md` allows: a half-applied `SetMeta`, a partly
//!   emptied recursive delete);
//! - any path the model does not predict must be a `.duet-partial-*` file
//!   belonging to an in-flight step, holding a prefix of its source.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use duet_ops::{JournalRecord, Plan, RemoveMode, Step, StepOutcome};
use duet_types::VPath;

pub const PARTIAL_PREFIX: &str = ".duet-partial-";

pub fn real(path: &VPath) -> PathBuf {
    PathBuf::from(path.inner().as_str())
}

/// One filesystem entry as the model sees it. `None` in an attribute means
/// "the model makes no claim" -- a freshly copied file's mode before its
/// `SetMeta` runs, a directory's mtime once something was created inside
/// it -- and matches anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Dir {
        mode: Option<u32>,
        mtime: Option<(i64, u32)>,
    },
    File {
        len: u64,
        hash: [u8; 32],
        mode: Option<u32>,
        mtime: Option<(i64, u32)>,
    },
    Symlink {
        target: String,
    },
}

impl Node {
    fn loosened(&self) -> Node {
        match self {
            Node::Dir { .. } => Node::Dir {
                mode: None,
                mtime: None,
            },
            Node::File { len, hash, .. } => Node::File {
                len: *len,
                hash: *hash,
                mode: None,
                mtime: None,
            },
            Node::Symlink { target } => Node::Symlink {
                target: target.clone(),
            },
        }
    }

    fn without_mtime(&self) -> Node {
        match self {
            Node::Dir { mode, .. } => Node::Dir {
                mode: *mode,
                mtime: None,
            },
            other => other.clone(),
        }
    }

    /// `self` is the expectation (wildcards allowed), `actual` what was
    /// captured from disk (every attribute known).
    fn admits(&self, actual: &Node) -> bool {
        fn attr<T: PartialEq>(want: &Option<T>, got: &Option<T>) -> bool {
            want.is_none() || want == got
        }
        match (self, actual) {
            (
                Node::Dir { mode, mtime },
                Node::Dir {
                    mode: actual_mode,
                    mtime: actual_mtime,
                },
            ) => attr(mode, actual_mode) && attr(mtime, actual_mtime),
            (
                Node::File {
                    len,
                    hash,
                    mode,
                    mtime,
                },
                Node::File {
                    len: actual_len,
                    hash: actual_hash,
                    mode: actual_mode,
                    mtime: actual_mtime,
                },
            ) => {
                len == actual_len
                    && hash == actual_hash
                    && attr(mode, actual_mode)
                    && attr(mtime, actual_mtime)
            }
            (Node::Symlink { target }, Node::Symlink { target: actual }) => target == actual,
            _ => false,
        }
    }
}

/// A snapshot (or prediction) of everything under a set of roots, keyed by
/// absolute path. The roots themselves are included.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tree {
    pub nodes: BTreeMap<PathBuf, Node>,
    /// `(dev, ino)` per regular file: which paths are names for the same
    /// inode. Captured from disk, and maintained by [`Tree::apply`] (a
    /// copy gets a fresh identity, a link shares its source's), because
    /// metadata belongs to the inode: a `SetMeta` through one name of a
    /// hardlinked file changes what every other name shows. Not part of
    /// node comparison.
    pub inodes: BTreeMap<PathBuf, (u64, u64)>,
    next_synthetic_inode: u64,
}

impl Tree {
    /// Captures every entry under `roots` exactly as it is on disk.
    pub fn capture(roots: &[PathBuf]) -> Tree {
        let mut tree = Tree::default();
        for root in roots {
            tree.capture_into(root);
        }
        tree
    }

    fn capture_into(&mut self, path: &Path) {
        let meta = std::fs::symlink_metadata(path)
            .unwrap_or_else(|e| panic!("capture: lstat {}: {e}", path.display()));
        let mtime = Some((meta.mtime(), meta.mtime_nsec() as u32));
        let mode = Some(meta.permissions().mode() & 0o7777);
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(path).unwrap();
            self.nodes.insert(
                path.to_path_buf(),
                Node::Symlink {
                    target: target.to_string_lossy().into_owned(),
                },
            );
        } else if meta.is_dir() {
            self.nodes
                .insert(path.to_path_buf(), Node::Dir { mode, mtime });
            let mut children: Vec<PathBuf> = std::fs::read_dir(path)
                .unwrap_or_else(|e| panic!("capture: read_dir {}: {e}", path.display()))
                .map(|entry| entry.unwrap().path())
                .collect();
            children.sort();
            for child in children {
                self.capture_into(&child);
            }
        } else {
            let bytes = std::fs::read(path)
                .unwrap_or_else(|e| panic!("capture: read {}: {e}", path.display()));
            self.nodes.insert(
                path.to_path_buf(),
                Node::File {
                    len: bytes.len() as u64,
                    hash: *blake3::hash(&bytes).as_bytes(),
                    mode,
                    mtime,
                },
            );
            self.inodes
                .insert(path.to_path_buf(), (meta.dev(), meta.ino()));
        }
    }

    pub fn get(&self, path: &Path) -> Option<&Node> {
        self.nodes.get(path)
    }

    fn subtree(&self, root: &Path) -> Vec<PathBuf> {
        self.nodes
            .keys()
            .filter(|p| p.starts_with(root))
            .cloned()
            .collect()
    }

    /// Every path that names the same inode as `path`, itself included.
    pub fn names_of(&self, path: &Path) -> Vec<PathBuf> {
        match self.inodes.get(path) {
            Some(inode) => self
                .inodes
                .iter()
                .filter(|(_, other)| *other == inode)
                .map(|(name, _)| name.clone())
                .collect(),
            None => vec![path.to_path_buf()],
        }
    }

    fn fresh_inode(&mut self) -> (u64, u64) {
        self.next_synthetic_inode += 1;
        (u64::MAX, self.next_synthetic_inode)
    }

    fn touch_parent(&mut self, path: &Path) {
        if let Some(parent) = path.parent()
            && let Some(node) = self.nodes.get(parent)
        {
            let loosened = node.without_mtime();
            self.nodes.insert(parent.to_path_buf(), loosened);
        }
    }

    /// Predicts `step`'s effect, assuming it succeeds with no conflict.
    pub fn apply(&mut self, step: &Step) {
        match step {
            Step::CreateDir { dest, .. } => {
                let dest = real(dest);
                if !self.nodes.contains_key(&dest) {
                    self.nodes.insert(
                        dest.clone(),
                        Node::Dir {
                            mode: None,
                            mtime: None,
                        },
                    );
                    self.touch_parent(&dest);
                }
            }
            Step::CopyFile { source, dest, .. } | Step::Reflink { source, dest, .. } => {
                let (source, dest) = (real(source), real(dest));
                let node = self
                    .nodes
                    .get(&source)
                    .unwrap_or_else(|| panic!("model: copy source {} missing", source.display()))
                    .loosened();
                self.nodes.insert(dest.clone(), node);
                let inode = self.fresh_inode();
                self.inodes.insert(dest.clone(), inode);
                self.touch_parent(&dest);
            }
            Step::Rename { source, dest, .. } => {
                let (source, dest) = (real(source), real(dest));
                for old in self.subtree(&source) {
                    let node = self.nodes.remove(&old).unwrap();
                    let relative = old.strip_prefix(&source).unwrap();
                    let new = if relative.as_os_str().is_empty() {
                        dest.clone()
                    } else {
                        dest.join(relative)
                    };
                    // A rename changes neither the entry's content nor its
                    // own attributes, but a renamed *directory's* mtime is
                    // left alone only on some filesystems.
                    if let Some(inode) = self.inodes.remove(&old) {
                        self.inodes.insert(new.clone(), inode);
                    }
                    self.nodes.insert(new, node.without_mtime());
                }
                self.touch_parent(&source);
                self.touch_parent(&dest);
            }
            Step::Link { source, dest, .. } => {
                let (source, dest) = (real(source), real(dest));
                let node = self
                    .nodes
                    .get(&source)
                    .unwrap_or_else(|| panic!("model: link source {} missing", source.display()))
                    .loosened();
                self.nodes.insert(dest.clone(), node);
                if let Some(inode) = self.inodes.get(&source).copied() {
                    self.inodes.insert(dest.clone(), inode);
                }
                self.touch_parent(&dest);
            }
            Step::Symlink {
                target, link_path, ..
            } => {
                let link = real(link_path);
                self.nodes.insert(
                    link.clone(),
                    Node::Symlink {
                        target: target.clone(),
                    },
                );
                self.touch_parent(&link);
            }
            Step::SetMeta { target, patch, .. } => {
                for name in self.names_of(&real(target)) {
                    if let Some(node) = self.nodes.get(&name).cloned() {
                        self.nodes
                            .insert(name, patched(&node, patch.mode, patch.modified));
                    }
                }
            }
            Step::Remove { target, mode, .. } => {
                let target = real(target);
                match mode {
                    RemoveMode::File | RemoveMode::EmptyDir => {
                        self.nodes.remove(&target);
                        self.inodes.remove(&target);
                    }
                    RemoveMode::Recursive => {
                        for path in self.subtree(&target) {
                            self.nodes.remove(&path);
                            self.inodes.remove(&path);
                        }
                    }
                }
                self.touch_parent(&target);
            }
            Step::Verify { .. } => {}
            Step::WriteTrashInfo { info_path, content } => {
                let path = real(info_path);
                self.nodes.insert(
                    path.clone(),
                    Node::File {
                        len: content.len() as u64,
                        hash: *blake3::hash(content.as_bytes()).as_bytes(),
                        mode: None,
                        mtime: None,
                    },
                );
                let inode = self.fresh_inode();
                self.inodes.insert(path.clone(), inode);
                self.touch_parent(&path);
            }
        }
    }

    /// The same tree with no claim about any directory's mtime -- for
    /// "nothing changed" comparisons across a job that legitimately
    /// created and removed scratch files inside those directories.
    pub fn ignoring_dir_mtimes(&self) -> Tree {
        let mut tree = self.clone();
        for node in tree.nodes.values_mut() {
            *node = node.without_mtime();
        }
        tree
    }

    pub fn apply_all(&mut self, plan: &Plan) {
        for step in &plan.steps {
            self.apply(step);
        }
    }

    /// Every distinct file content in the tree, ignoring scratch files.
    pub fn contents(&self) -> BTreeSet<[u8; 32]> {
        self.nodes
            .iter()
            .filter(|(path, _)| !is_partial(path))
            .filter_map(|(_, node)| match node {
                Node::File { hash, .. } => Some(*hash),
                _ => None,
            })
            .collect()
    }

    pub fn partials(&self) -> Vec<PathBuf> {
        self.nodes
            .keys()
            .filter(|path| is_partial(path))
            .cloned()
            .collect()
    }
}

fn patched(node: &Node, mode: Option<u32>, modified: Option<duet_types::Timestamp>) -> Node {
    let new_mtime = modified.map(|t| (t.secs, t.nanos));
    match node {
        Node::Dir {
            mode: old_mode,
            mtime,
        } => Node::Dir {
            mode: mode.map(|m| m & 0o7777).or(*old_mode),
            mtime: new_mtime.or(*mtime),
        },
        Node::File {
            len,
            hash,
            mode: old_mode,
            mtime,
        } => Node::File {
            len: *len,
            hash: *hash,
            mode: mode.map(|m| m & 0o7777).or(*old_mode),
            mtime: new_mtime.or(*mtime),
        },
        Node::Symlink { .. } => node.clone(),
    }
}

pub fn is_partial(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(PARTIAL_PREFIX))
}

/// What the journal durably says about one step.
#[derive(Debug, Clone, PartialEq)]
pub enum StepStatus {
    /// No `Intent`: by the journal's contract the step has had no effect.
    Untouched,
    /// `Intent` without `Completion`: the crash window. `partials` are
    /// the staging names every attempt's `Intent` recorded.
    InFlight {
        partials: Vec<String>,
    },
    Succeeded,
    /// A durable `Skipped` completion: the step decided not to act, and
    /// must have had no effect at all.
    Skipped,
    /// A durable `Failed` completion. A step that failed may have got part
    /// of the way (or, for a failure after its real work, all of the way):
    /// like an in-flight step it is legal in its before-state, its
    /// after-state, or the kind's own intermediate states -- and nothing
    /// else.
    Failed {
        partials: Vec<String>,
    },
}

impl StepStatus {
    /// The step may have acted, wholly or partly.
    fn may_have_acted(&self) -> Option<&[String]> {
        match self {
            StepStatus::InFlight { partials } | StepStatus::Failed { partials } => Some(partials),
            _ => None,
        }
    }
}

/// Reads one job's journal the way a crash leaves it: every parseable
/// record, a torn tail ignored. Deliberately not `JournalReader::scan` --
/// the checker needs to know *which* steps completed and how, and it must
/// not depend on the code under test to tell it.
pub fn read_journal(state_dir: &Path, job_id: u64) -> Vec<JournalRecord> {
    let path = state_dir.join("jobs").join(format!("{job_id}.journal"));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.is_empty())
        .map_while(|line| serde_json::from_str::<JournalRecord>(line).ok())
        .collect()
}

pub fn step_statuses(plan: &Plan, records: &[JournalRecord]) -> Vec<StepStatus> {
    let mut statuses = vec![StepStatus::Untouched; plan.steps.len()];
    let mut partials: Vec<Vec<String>> = vec![Vec::new(); plan.steps.len()];
    for record in records {
        match record {
            JournalRecord::Intent {
                step_index,
                partial_name,
                ..
            } => {
                let index = *step_index as usize;
                partials[index].extend(partial_name.clone());
                statuses[index] = StepStatus::InFlight {
                    partials: partials[index].clone(),
                };
            }
            JournalRecord::Completion {
                step_index,
                outcome,
            } => {
                let index = *step_index as usize;
                statuses[index] = match outcome {
                    StepOutcome::Succeeded => StepStatus::Succeeded,
                    StepOutcome::Skipped { .. } => StepStatus::Skipped,
                    StepOutcome::Failed(_) => StepStatus::Failed {
                        partials: partials[index].clone(),
                    },
                };
            }
            _ => {}
        }
    }
    statuses
}

/// The per-path set of states an interrupted job may legally have left.
struct Allowed {
    base: Tree,
    alternatives: BTreeMap<PathBuf, Vec<Option<Node>>>,
    /// `(source, dest)` of every `Link` step caught in flight: `dest` may
    /// already be another name for `source`'s inode.
    links_in_flight: Vec<(PathBuf, PathBuf)>,
}

impl Allowed {
    fn options(&self, path: &Path) -> Vec<Option<Node>> {
        self.alternatives
            .get(path)
            .cloned()
            .unwrap_or_else(|| vec![self.base.get(path).cloned()])
    }

    fn allow(&mut self, path: &Path, node: Option<Node>) {
        let mut options = self.options(path);
        if !options.contains(&node) {
            options.push(node);
        }
        self.alternatives.insert(path.to_path_buf(), options);
    }

    /// Every path that names, or may by now name, the same inode as `path`.
    fn names_of(&self, path: &Path) -> Vec<PathBuf> {
        let mut names = self.base.names_of(path);
        loop {
            let mut grew = false;
            for (source, dest) in &self.links_in_flight {
                let has_source = names.contains(source);
                let has_dest = names.contains(dest);
                if has_source != has_dest {
                    for extra in self.base.names_of(if has_source { dest } else { source }) {
                        if !names.contains(&extra) {
                            names.push(extra);
                        }
                    }
                    grew = true;
                }
            }
            if !grew {
                return names;
            }
        }
    }

    /// Whatever state `path`'s parent directory is allowed to be in, it may
    /// also have a different mtime.
    fn allow_parent_touched(&mut self, path: &Path) {
        if let Some(parent) = path.parent() {
            for node in self.options(parent).into_iter().flatten() {
                self.allow(parent, Some(node.without_mtime()));
            }
        }
    }

    /// Widens the allowed states by "this in-flight step may also have
    /// happened", wholly or (where the kind genuinely has intermediate
    /// states) in part.
    fn allow_step(&mut self, step: &Step) {
        match step {
            Step::CreateDir { dest, .. } => {
                let dest = real(dest);
                self.allow(
                    &dest,
                    Some(Node::Dir {
                        mode: None,
                        mtime: None,
                    }),
                );
                self.allow_parent_touched(&dest);
            }
            Step::CopyFile { source, dest, .. }
            | Step::Reflink { source, dest, .. }
            | Step::Link { source, dest, .. } => {
                let (source, dest) = (real(source), real(dest));
                for option in self.options(&source).into_iter().flatten() {
                    self.allow(&dest, Some(option.loosened()));
                }
                self.allow_parent_touched(&dest);
            }
            Step::Rename { source, dest, .. } => {
                let (source, dest) = (real(source), real(dest));
                for old in self.base.subtree(&source) {
                    let relative = old.strip_prefix(&source).unwrap().to_path_buf();
                    let new = if relative.as_os_str().is_empty() {
                        dest.clone()
                    } else {
                        dest.join(&relative)
                    };
                    for option in self.options(&old).into_iter().flatten() {
                        self.allow(&new, Some(option.without_mtime()));
                    }
                    self.allow(&old, None);
                }
                self.allow_parent_touched(&source);
                self.allow_parent_touched(&dest);
            }
            Step::Symlink {
                target, link_path, ..
            } => {
                let link = real(link_path);
                self.allow(
                    &link,
                    Some(Node::Symlink {
                        target: target.clone(),
                    }),
                );
                self.allow_parent_touched(&link);
            }
            Step::SetMeta { target, patch, .. } => {
                // Genuinely multi-phase (mode, then xattrs, then times):
                // "mode applied, times not yet" is a legal crash state.
                // Metadata lives on the inode: every name of it shows the
                // change.
                for name in self.names_of(&real(target)) {
                    for option in self.options(&name).into_iter().flatten() {
                        self.allow(&name, Some(patched(&option, patch.mode, None)));
                        self.allow(&name, Some(patched(&option, patch.mode, patch.modified)));
                    }
                }
            }
            Step::Remove { target, mode, .. } => {
                let target = real(target);
                match mode {
                    RemoveMode::File | RemoveMode::EmptyDir => self.allow(&target, None),
                    RemoveMode::Recursive => {
                        // The one legitimately partial state: any subset of
                        // the subtree may be gone, and a directory that
                        // lost children has a new mtime.
                        for path in self.base.subtree(&target) {
                            for option in self.options(&path).into_iter().flatten() {
                                self.allow(&path, Some(option.without_mtime()));
                            }
                            self.allow(&path, None);
                        }
                    }
                }
                self.allow_parent_touched(&target);
            }
            Step::Verify { .. } => {}
            Step::WriteTrashInfo { info_path, content } => {
                let path = real(info_path);
                self.allow(
                    &path,
                    Some(Node::File {
                        len: content.len() as u64,
                        hash: *blake3::hash(content.as_bytes()).as_bytes(),
                        mode: None,
                        mtime: None,
                    }),
                );
                self.allow_parent_touched(&path);
            }
        }
    }
}

/// Checks the §14.4 invariant for a job that did not run to completion.
///
/// `before` is the tree captured before the job; `statuses` is what the
/// journal durably records; `after` is the tree as the fault left it.
pub fn check_interrupted(
    before: &Tree,
    plan: &Plan,
    statuses: &[StepStatus],
    after: &Tree,
) -> Result<(), String> {
    let mut base = before.clone();
    for (step, status) in plan.steps.iter().zip(statuses) {
        if *status == StepStatus::Succeeded {
            base.apply(step);
        }
    }
    let links_in_flight = plan
        .steps
        .iter()
        .zip(statuses)
        .filter_map(|(step, status)| match (step, status) {
            (Step::Link { source, dest, .. }, status) if status.may_have_acted().is_some() => {
                Some((real(source), real(dest)))
            }
            _ => None,
        })
        .collect();
    let mut allowed = Allowed {
        base,
        alternatives: BTreeMap::new(),
        links_in_flight,
    };
    for (step, status) in plan.steps.iter().zip(statuses) {
        if status.may_have_acted().is_some() {
            allowed.allow_step(step);
        }
    }

    let mut problems: Vec<String> = Vec::new();
    let mut paths: BTreeSet<&PathBuf> = after.nodes.keys().collect();
    paths.extend(allowed.base.nodes.keys());
    paths.extend(allowed.alternatives.keys());
    for path in paths {
        let actual = after.get(path);
        let options = allowed.options(path);
        let ok = options.iter().any(|option| match (option, actual) {
            (None, None) => true,
            (Some(want), Some(got)) => want.admits(got),
            _ => false,
        });
        if ok {
            continue;
        }
        if actual.is_some() && is_partial(path) {
            if let Err(why) = check_partial(path, plan, statuses) {
                problems.push(why);
            }
            continue;
        }
        problems.push(format!(
            "{}: found {:?}, allowed {:?}",
            path.display(),
            actual,
            options
        ));
    }

    // "Never both, never neither": a rename caught in flight has its entry
    // at exactly one of its two names. (Per-path alternatives alone would
    // accept "gone from both".)
    for (step, status) in plan.steps.iter().zip(statuses) {
        if let Step::Rename { source, dest, .. } = step
            && status.may_have_acted().is_some()
        {
            let (source, dest) = (real(source), real(dest));
            let pre_existing_dest = allowed.base.get(&dest).is_some();
            let at_source = after.get(&source).is_some();
            let at_dest = after.get(&dest).is_some();
            if !pre_existing_dest && at_source == at_dest {
                problems.push(format!(
                    "rename {} -> {}: entry present at source={at_source}, dest={at_dest}; \
                     must be at exactly one",
                    source.display(),
                    dest.display()
                ));
            }
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

/// A `.duet-partial-*` file is legal only as the staging file of a step
/// that was in flight or failed, and only if what it holds is a prefix of
/// that step's source -- "partial and marked", never an arbitrary stray.
fn check_partial(path: &Path, plan: &Plan, statuses: &[StepStatus]) -> Result<(), String> {
    let name = path.file_name().unwrap().to_str().unwrap();
    let parent = path.parent().unwrap();
    for (step, status) in plan.steps.iter().zip(statuses) {
        let Some(partials) = status.may_have_acted() else {
            continue;
        };
        match step {
            Step::CopyFile { source, dest, .. } | Step::Reflink { source, dest, .. } => {
                let dest = real(dest);
                // Either one of the executor's own journal-named partials
                // for this step, or the backend's staging sibling of one
                // (buffered fallback).
                let ours = partials.iter().any(|partial_name| {
                    name == partial_name || name.ends_with(&format!("-{partial_name}"))
                });
                if dest.parent() != Some(parent) || !ours {
                    continue;
                }
                let partial = std::fs::read(path).map_err(|e| e.to_string())?;
                let source = std::fs::read(real(source)).map_err(|e| e.to_string())?;
                return if source.starts_with(&partial) {
                    Ok(())
                } else {
                    Err(format!(
                        "{}: partial ({} bytes) is not a prefix of its source ({} bytes)",
                        path.display(),
                        partial.len(),
                        source.len()
                    ))
                };
            }
            Step::WriteTrashInfo { info_path, content } => {
                let info = real(info_path);
                let info_name = info.file_name().unwrap().to_str().unwrap();
                if info.parent() != Some(parent) || !name.ends_with(&format!("-{info_name}")) {
                    continue;
                }
                let partial = std::fs::read(path).map_err(|e| e.to_string())?;
                return if content.as_bytes().starts_with(&partial) {
                    Ok(())
                } else {
                    Err(format!(
                        "{}: staged trashinfo is not a prefix of the planned content",
                        path.display()
                    ))
                };
            }
            _ => {}
        }
    }
    Err(format!(
        "{}: a .duet-partial-* file no in-flight step accounts for",
        path.display()
    ))
}

/// Checks that `after` is exactly what running `plan` to completion on
/// `before` produces: every destination complete, nothing extra, and no
/// scratch file left anywhere.
pub fn check_finished(before: &Tree, plan: &Plan, after: &Tree) -> Result<(), String> {
    let mut expected = before.clone();
    expected.apply_all(plan);
    compare(&expected, after)
}

/// Exact comparison (wildcard attributes aside) of a prediction with a
/// captured tree.
pub fn compare(expected: &Tree, actual: &Tree) -> Result<(), String> {
    let mut problems: Vec<String> = Vec::new();
    let mut paths: BTreeSet<&PathBuf> = expected.nodes.keys().collect();
    paths.extend(actual.nodes.keys());
    for path in paths {
        match (expected.get(path), actual.get(path)) {
            (Some(want), Some(got)) if want.admits(got) => {}
            (want, got) => problems.push(format!(
                "{}: expected {:?}, found {:?}",
                path.display(),
                want,
                got
            )),
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

/// The journal-independent half of the invariant: no file content that
/// existed before the job may be gone afterwards, except content the job
/// was explicitly asked to destroy (`deletable` prefixes).
pub fn check_no_content_lost(
    before: &Tree,
    after: &Tree,
    deletable: &[PathBuf],
) -> Result<(), String> {
    let surviving = after.contents();
    let mut lost: Vec<String> = Vec::new();
    for (path, node) in &before.nodes {
        let Node::File { hash, .. } = node else {
            continue;
        };
        if is_partial(path) || deletable.iter().any(|prefix| path.starts_with(prefix)) {
            continue;
        }
        if !surviving.contains(hash) {
            lost.push(path.display().to_string());
        }
    }
    if lost.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "content that existed before the job exists nowhere after it: {}",
            lost.join(", ")
        ))
    }
}
