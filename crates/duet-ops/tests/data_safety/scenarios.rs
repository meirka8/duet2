// SPDX-License-Identifier: MIT
//! Fixtures: a tree on disk plus the `Plan` to run against it.
//!
//! Single-step scenarios (one per `Step` kind) back the row-by-row matrix;
//! the tree scenarios go through the real planners, so the sweeps exercise
//! the plans the product actually produces.

use std::path::{Path, PathBuf};

use duet_ops::{
    CancelToken, ConflictPolicy, DeleteMode, JobKind, Plan, PlanOptions, RemoveMode, Step,
    VerifyAlgorithm, plan_copy, plan_delete, plan_move,
};
use duet_types::{MetaPatch, Timestamp};
use duet_vfs::LocalFs;

use crate::harness::{
    Backend, ChildSpec, JOB, World, block_on, pattern, set_mtime, vp, write_file,
};

/// A fixture ready to run.
pub struct Scenario {
    pub plan: Plan,
    pub kind: JobKind,
    /// Subtrees the job is *asked* to destroy: content under them is not
    /// "lost" when it goes away.
    pub deletable: Vec<PathBuf>,
}

impl Scenario {
    fn new(kind: JobKind, steps: Vec<Step>) -> Scenario {
        Scenario {
            plan: Plan::new(steps, PlanOptions::default()),
            kind,
            deletable: Vec::new(),
        }
    }

    pub fn spec(&self, world: &World, backend: Backend) -> ChildSpec {
        ChildSpec {
            plan: self.plan.clone(),
            kind: self.kind,
            job_id: JOB,
            state_dir: world.state.clone(),
            concurrency: 4,
            backend,
            linger: false,
        }
    }
}

const T0: u64 = 1_600_000_000;

/// `src/a.bin` -> `dst/a.bin`.
pub struct CopyOpts {
    pub size: usize,
    /// `Step::Reflink` instead of `Step::CopyFile`.
    pub reflink: bool,
    /// Put a different file at the destination first and plan an overwrite.
    pub replace_existing: bool,
}

impl CopyOpts {
    pub fn small() -> CopyOpts {
        CopyOpts {
            size: 4096,
            reflink: false,
            replace_existing: false,
        }
    }
    /// Three buffers' worth: the buffered fallback has a real mid-write.
    pub fn multi_chunk() -> CopyOpts {
        CopyOpts {
            size: 3 * 1024 * 1024 + 17,
            ..CopyOpts::small()
        }
    }
    pub fn reflink(mut self) -> CopyOpts {
        self.reflink = true;
        self
    }
    pub fn replacing(mut self) -> CopyOpts {
        self.replace_existing = true;
        self
    }
}

pub fn copy_file(world: &World, opts: CopyOpts) -> Scenario {
    let source = world.root.join("src/a.bin");
    let dest = world.root.join("dst/a.bin");
    write_file(&source, &pattern(1, opts.size));
    set_mtime(&source, T0);
    std::fs::create_dir_all(world.root.join("dst")).unwrap();
    let conflict = if opts.replace_existing {
        write_file(&dest, &pattern(2, 1500));
        set_mtime(&dest, T0 - 1000);
        Some(ConflictPolicy::Overwrite)
    } else {
        None
    };
    let step = if opts.reflink {
        Step::Reflink {
            source: vp(&source),
            dest: vp(&dest),
            size: opts.size as u64,
            conflict,
        }
    } else {
        Step::CopyFile {
            source: vp(&source),
            dest: vp(&dest),
            size: opts.size as u64,
            conflict,
        }
    };
    let mut scenario = Scenario::new(JobKind::Copy, vec![step]);
    if opts.replace_existing {
        // The job is asked to replace this file; its old bytes going away
        // is the operation, not a loss.
        scenario.deletable.push(dest);
    }
    scenario
}

/// `mkdir dst/new`.
pub fn create_dir(world: &World) -> Scenario {
    std::fs::create_dir_all(world.root.join("dst")).unwrap();
    Scenario::new(
        JobKind::CreateDir,
        vec![Step::CreateDir {
            dest: vp(&world.root.join("dst/new")),
            mode: Some(0o750),
        }],
    )
}

/// `src/a.bin` renamed to `dst/a.bin`, same filesystem.
pub fn rename(world: &World) -> Scenario {
    let source = world.root.join("src/a.bin");
    write_file(&source, &pattern(3, 9000));
    set_mtime(&source, T0);
    std::fs::create_dir_all(world.root.join("dst")).unwrap();
    Scenario::new(
        JobKind::Move,
        vec![Step::Rename {
            source: vp(&source),
            dest: vp(&world.root.join("dst/a.bin")),
            conflict: None,
            depends_on: None,
        }],
    )
}

/// `dst/a.link` hardlinked to `src/a.bin`.
pub fn link(world: &World) -> Scenario {
    let source = world.root.join("src/a.bin");
    write_file(&source, &pattern(4, 9000));
    set_mtime(&source, T0);
    std::fs::create_dir_all(world.root.join("dst")).unwrap();
    Scenario::new(
        JobKind::CreateHardlink,
        vec![Step::Link {
            source: vp(&source),
            dest: vp(&world.root.join("dst/a.link")),
            depends_on: None,
        }],
    )
}

/// `dst/a.sym` -> an absolute target.
pub fn symlink(world: &World) -> Scenario {
    let target = world.root.join("src/a.bin");
    write_file(&target, &pattern(5, 100));
    set_mtime(&target, T0);
    std::fs::create_dir_all(world.root.join("dst")).unwrap();
    Scenario::new(
        JobKind::CreateSymlink,
        vec![Step::Symlink {
            target: target.to_str().unwrap().to_string(),
            link_path: vp(&world.root.join("dst/a.sym")),
            depends_on: None,
        }],
    )
}

pub const SET_META_OLD_MODE: u32 = 0o644;
pub const SET_META_NEW_MODE: u32 = 0o600;
pub const SET_META_NEW_MTIME: i64 = 1_500_000_000;

/// chmod + set mtime on an existing file.
pub fn set_meta(world: &World) -> Scenario {
    use std::os::unix::fs::PermissionsExt;
    let target = world.root.join("dst/a.bin");
    write_file(&target, &pattern(6, 5000));
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(SET_META_OLD_MODE)).unwrap();
    set_mtime(&target, T0);
    Scenario::new(
        JobKind::ChangeAttributes,
        vec![Step::SetMeta {
            target: vp(&target),
            patch: MetaPatch {
                mode: Some(SET_META_NEW_MODE),
                modified: Some(Timestamp::new(SET_META_NEW_MTIME, 0)),
                ..MetaPatch::default()
            },
            depends_on: None,
        }],
    )
}

/// Removes one file.
pub fn remove_file(world: &World) -> Scenario {
    let target = world.root.join("src/a.bin");
    write_file(&target, &pattern(7, 5000));
    set_mtime(&target, T0);
    write_file(&world.root.join("src/keep.bin"), &pattern(8, 700));
    let mut scenario = Scenario::new(
        JobKind::Delete { permanent: true },
        vec![Step::Remove {
            target: vp(&target),
            mode: RemoveMode::File,
            depends_on: None,
        }],
    );
    scenario.deletable.push(target);
    scenario
}

/// Removes one empty directory.
pub fn remove_empty_dir(world: &World) -> Scenario {
    let target = world.root.join("src/empty");
    std::fs::create_dir_all(&target).unwrap();
    write_file(&world.root.join("src/keep.bin"), &pattern(9, 700));
    let mut scenario = Scenario::new(
        JobKind::Delete { permanent: true },
        vec![Step::Remove {
            target: vp(&target),
            mode: RemoveMode::EmptyDir,
            depends_on: None,
        }],
    );
    scenario.deletable.push(target);
    scenario
}

/// Recursively removes `src/tree` -- nine files in nested directories,
/// plus a symlink pointing *out* of the tree at `outside/precious.bin`,
/// which must survive whatever happens.
pub fn remove_tree(world: &World) -> Scenario {
    let target = world.root.join("src/tree");
    for i in 0..3u64 {
        for j in 0..3u64 {
            write_file(
                &target.join(format!("d{i}/f{j}.bin")),
                &pattern(100 + i * 10 + j, 300),
            );
        }
    }
    let outside = world.root.join("outside");
    write_file(&outside.join("precious.bin"), &pattern(200, 4000));
    std::os::unix::fs::symlink(&outside, target.join("d1/escape")).unwrap();
    write_file(&world.root.join("src/sibling.bin"), &pattern(201, 800));
    let mut scenario = Scenario::new(
        JobKind::Delete { permanent: true },
        vec![Step::Remove {
            target: vp(&target),
            mode: RemoveMode::Recursive,
            depends_on: None,
        }],
    );
    scenario.deletable.push(target);
    scenario
}

/// Verifies `dst/a.bin` against `src/a.bin`. With `matching == false` the
/// destination differs, and a `Remove` of the source gated on the verify
/// follows -- the cross-device move's "never unlink a source whose copy
/// did not verify".
pub fn verify(world: &World, matching: bool) -> Scenario {
    let source = world.root.join("src/a.bin");
    let dest = world.root.join("dst/a.bin");
    let size = 3 * 1024 * 1024 + 5;
    write_file(&source, &pattern(10, size));
    set_mtime(&source, T0);
    write_file(&dest, &pattern(if matching { 10 } else { 11 }, size));
    set_mtime(&dest, T0);
    let mut steps = vec![Step::Verify {
        source: vp(&source),
        dest: vp(&dest),
        algorithm: VerifyAlgorithm::Blake3,
        depends_on: None,
    }];
    if !matching {
        steps.push(Step::Remove {
            target: vp(&source),
            mode: RemoveMode::File,
            depends_on: Some(0),
        });
    }
    Scenario::new(JobKind::Move, steps)
}

/// The trash pair: write `Trash/info/a.bin.trashinfo`, then -- gated on it
/// -- move `src/a.bin` to `Trash/files/a.bin`.
pub fn trash_one(world: &World) -> Scenario {
    let source = world.root.join("src/a.bin");
    write_file(&source, &pattern(12, 6000));
    set_mtime(&source, T0);
    let trash = world.root.join("data/Trash");
    std::fs::create_dir_all(trash.join("info")).unwrap();
    std::fs::create_dir_all(trash.join("files")).unwrap();
    let content = format!(
        "[Trash Info]\nPath={}\nDeletionDate=2026-11-22T12:00:00\n",
        source.display()
    );
    let mut scenario = Scenario::new(
        JobKind::Delete { permanent: false },
        vec![
            Step::WriteTrashInfo {
                info_path: vp(&trash.join("info/a.bin.trashinfo")),
                content,
            },
            Step::Rename {
                source: vp(&source),
                dest: vp(&trash.join("files/a.bin")),
                conflict: None,
                depends_on: Some(0),
            },
        ],
    );
    scenario.deletable.clear();
    scenario
}

/// A source tree with everything a copy has to get right: nested
/// directories, an empty directory, files from empty to a few hundred KiB,
/// a hardlinked pair, distinct modes and pinned mtimes.
pub fn build_source_tree(src: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let files: [(&str, u64, usize, u32); 7] = [
        ("top.txt", 20, 1200, 0o644),
        ("empty.dat", 21, 0, 0o600),
        ("docs/readme.md", 22, 5000, 0o640),
        ("docs/big.bin", 23, 300_000, 0o644),
        ("docs/deep/a.bin", 24, 70_000, 0o755),
        ("docs/deep/b.bin", 25, 9, 0o444),
        ("media/clip.raw", 26, 150_000, 0o644),
    ];
    for (i, (name, seed, len, mode)) in files.iter().enumerate() {
        let path = src.join(name);
        write_file(&path, &pattern(*seed, *len));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(*mode)).unwrap();
        set_mtime(&path, T0 + i as u64 * 60);
    }
    std::fs::hard_link(src.join("docs/readme.md"), src.join("media/readme-link.md")).unwrap();
    std::fs::create_dir_all(src.join("hollow")).unwrap();
    // Directory mtimes last, deepest first, so pinning one does not
    // disturb another.
    for (i, dir) in ["docs/deep", "docs", "media", "hollow", ""]
        .iter()
        .enumerate()
    {
        set_mtime(&src.join(dir), T0 - 5000 + i as u64 * 60);
    }
}

fn options(verify: bool) -> PlanOptions {
    PlanOptions {
        verify,
        ..PlanOptions::default()
    }
}

/// F5 on a directory: `plan_copy` of the source tree into `dst/`.
pub fn copy_tree(world: &World, verify: bool) -> Scenario {
    let src = world.root.join("src");
    build_source_tree(&src);
    let dst = world.root.join("dst");
    std::fs::create_dir_all(&dst).unwrap();
    let plan = block_on(plan_copy(
        &LocalFs,
        &[vp(&src)],
        &vp(&dst),
        options(verify),
        &CancelToken::new(),
    ))
    .expect("plan_copy");
    Scenario {
        plan,
        kind: JobKind::Copy,
        deletable: Vec::new(),
    }
}

/// F6 across filesystems: `plan_move` of the source tree onto the world's
/// second device -- copy, (verify,) then remove each source.
pub fn move_tree_across_devices(world: &World, verify: bool) -> Scenario {
    let src = world.root.join("src");
    build_source_tree(&src);
    let dst = world.far().join("dst");
    std::fs::create_dir_all(&dst).unwrap();
    let plan = block_on(plan_move(
        &LocalFs,
        &[vp(&src)],
        &vp(&dst),
        options(verify),
        &CancelToken::new(),
    ))
    .expect("plan_move");
    assert!(
        plan.steps
            .iter()
            .any(|step| matches!(step, Step::CopyFile { .. } | Step::Reflink { .. })),
        "a cross-device move must plan copies, not renames: {:?}",
        plan.steps
    );
    Scenario {
        plan,
        kind: JobKind::Move,
        deletable: Vec::new(),
    }
}

/// F6 within one filesystem: a single rename per source.
pub fn move_tree_same_device(world: &World) -> Scenario {
    let src = world.root.join("src");
    build_source_tree(&src);
    write_file(&world.root.join("loose.bin"), &pattern(30, 2222));
    let dst = world.root.join("dst");
    std::fs::create_dir_all(&dst).unwrap();
    let plan = block_on(plan_move(
        &LocalFs,
        &[vp(&src), vp(&world.root.join("loose.bin"))],
        &vp(&dst),
        options(false),
        &CancelToken::new(),
    ))
    .expect("plan_move");
    Scenario {
        plan,
        kind: JobKind::Move,
        deletable: Vec::new(),
    }
}

/// Shift+Del: permanent delete of the source tree and one loose file.
pub fn delete_tree(world: &World) -> Scenario {
    let src = world.root.join("src");
    build_source_tree(&src);
    let loose = world.root.join("loose.bin");
    write_file(&loose, &pattern(31, 2222));
    write_file(&world.root.join("bystander.bin"), &pattern(32, 3333));
    let plan = block_on(plan_delete(
        &LocalFs,
        &[vp(&src), vp(&loose)],
        DeleteMode::Permanent,
        options(false),
        &CancelToken::new(),
    ))
    .expect("plan_delete");
    Scenario {
        plan,
        kind: JobKind::Delete { permanent: true },
        deletable: vec![src, loose],
    }
}

/// F8: the source tree and one loose file moved to the trash.
pub fn trash_tree(world: &World) -> Scenario {
    let src = world.root.join("src");
    build_source_tree(&src);
    let loose = world.root.join("loose.bin");
    write_file(&loose, &pattern(33, 2222));
    write_file(&world.root.join("bystander.bin"), &pattern(34, 3333));
    let data_home = world.root.join("data");
    std::fs::create_dir_all(&data_home).unwrap();
    let plan = block_on(plan_delete(
        &LocalFs,
        &[vp(&src), vp(&loose)],
        DeleteMode::Trash {
            data_home: vp(&data_home),
        },
        options(false),
        &CancelToken::new(),
    ))
    .expect("plan_delete (trash)");
    Scenario {
        plan,
        kind: JobKind::Delete { permanent: false },
        deletable: Vec::new(),
    }
}

/// `count` steps of one atomic kind, for kills at arbitrary instants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StormKind {
    Copy { reflink: bool },
    CreateDir,
    Rename,
    Link,
    RemoveFile,
    RemoveEmptyDir,
}

pub fn storm(world: &World, kind: StormKind, count: usize) -> Scenario {
    let src = world.root.join("src");
    let dst = world.root.join("dst");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dst).unwrap();
    let mut steps = Vec::with_capacity(count);
    let mut deletable = Vec::new();
    for i in 0..count {
        let name = format!("f{i:05}");
        let source = src.join(&name);
        match kind {
            StormKind::Copy { .. }
            | StormKind::Rename
            | StormKind::Link
            | StormKind::RemoveFile => {
                std::fs::write(&source, pattern(1000 + i as u64, 64 + i % 200)).unwrap();
            }
            StormKind::RemoveEmptyDir => std::fs::create_dir(&source).unwrap(),
            StormKind::CreateDir => {}
        }
        steps.push(match kind {
            StormKind::Copy { reflink: false } => Step::CopyFile {
                source: vp(&source),
                dest: vp(&dst.join(&name)),
                size: (64 + i % 200) as u64,
                conflict: None,
            },
            StormKind::Copy { reflink: true } => Step::Reflink {
                source: vp(&source),
                dest: vp(&dst.join(&name)),
                size: (64 + i % 200) as u64,
                conflict: None,
            },
            StormKind::CreateDir => Step::CreateDir {
                dest: vp(&dst.join(&name)),
                mode: None,
            },
            StormKind::Rename => Step::Rename {
                source: vp(&source),
                dest: vp(&dst.join(&name)),
                conflict: None,
                depends_on: None,
            },
            StormKind::Link => Step::Link {
                source: vp(&source),
                dest: vp(&dst.join(&name)),
                depends_on: None,
            },
            StormKind::RemoveFile => Step::Remove {
                target: vp(&source),
                mode: RemoveMode::File,
                depends_on: None,
            },
            StormKind::RemoveEmptyDir => Step::Remove {
                target: vp(&source),
                mode: RemoveMode::EmptyDir,
                depends_on: None,
            },
        });
    }
    let job_kind = match kind {
        StormKind::Copy { .. } => JobKind::Copy,
        StormKind::CreateDir => JobKind::CreateDir,
        StormKind::Rename => JobKind::Move,
        StormKind::Link => JobKind::CreateHardlink,
        StormKind::RemoveFile | StormKind::RemoveEmptyDir => {
            deletable.push(src.clone());
            JobKind::Delete { permanent: true }
        }
    };
    let mut scenario = Scenario::new(job_kind, steps);
    scenario.deletable = deletable;
    scenario
}
