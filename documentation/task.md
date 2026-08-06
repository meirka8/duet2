# Duet — Work Breakdown Structure & Task Plan

| Field | Value |
|---|---|
| Document ID | DUET-WBS-001 |
| Version | 0.1 (Draft) |
| Companion | `design.md` (DUET-DD-001) |
| Date | 2026-08-02 |
| Method | Waterfall with phase gates; each phase has entry criteria, tasks, and a Definition of Done |

## How to read this document

- **ID** — stable task identifier `T-<phase>.<group>.<n>`. Spikes are `S-n`. Never renumber; append.
- **Est** — ideal engineer-days: uninterrupted, context-loaded, no meetings. Multiply by 1.6–2.2 for calendar reality on a side project.
- **Deps** — tasks that must be *complete*, not merely started.
- **Traces** — requirement IDs from `design.md` §6. Every task traces to at least one requirement or is infrastructure (marked `INFRA`).
- **AC** — acceptance criteria. A task is done when its AC are demonstrably true, not when the code compiles.

**Global Definition of Done** (applies to every implementation task, in addition to its own AC):
1. Code merged to `main` behind CI green (build, clippy `-D warnings`, `rustfmt`, tests).
2. Unit tests written for new logic; integration test if it crosses a crate boundary.
3. No new blocking call on the UI thread (enforced by the T-3.1.6 guard).
4. Public items documented; anything user-visible reflected in `docs/`.
5. If it touches file operations: a case added to the data-safety suite (T-10.2.1).
6. If it touches a hot path: a bench added or updated, baseline recorded.

**Effort summary**

| Phase | Title | Est (ideal days) |
|---|---|---|
| 0 | Feasibility & spikes | 21 |
| 1 | Requirements baseline | 14 |
| 2 | Architecture & detailed design | 20 |
| 3 | Core foundations | 66 |
| 4 | UI shell — walking skeleton (M1) | 58 |
| 5 | Operations engine & TC parity (M2 / Alpha) | 104 |
| 6 | Archives & VFS expansion | 52 |
| 7 | Remote backends | 46 |
| 8 | Plugin system | 58 |
| 9 | Tools, polish, i18n, a11y | 74 |
| 10 | Hardening & QA (M4 / RC) | 56 |
| 11 | Packaging & release (M5 / 1.0) | 30 |
| — | Contingency @ 15% | 90 |
| **Total** | | **≈ 689** |

> The reduced-scope path in `design.md` §16.1 (Phases 0–5 + tasks 6.1–6.3 + a trimmed Phase 9/10/11) totals **≈ 240 days including contingency**. If this is a solo evenings-and-weekends project, plan that one and treat everything past Phase 5 as a post-1.0 roadmap.

---

# Phase 0 — Feasibility & spikes

**Objective:** discover, before committing to ADR-001, whether GPUI can carry the parts of a file manager that an editor never needed. Every spike is timeboxed and produces a written verdict; a spike that overruns its box is itself a finding.

**Entry criteria:** none.
**Exit criteria (gate G0):** all spikes have a written verdict; S-1, S-2, S-3 are green or have a costed workaround; ADR-001 confirmed, amended, or reversed.

| ID | Spike | Timebox | AC | Traces |
|---|---|---|---|---|
| S-1 | **Virtualised table at scale.** Build a throwaway GPUI app rendering a 1,000,000-row table from struct-of-arrays backing data using `gpui-component`'s `Table`, with sorting, a cursor, multi-selection highlight, and 5 columns. | 4 d | Frame time under 8.3 ms while scrolling at speed on a mid-range GPU; sort of 1M rows under 400 ms; RSS under 300 MB; a written note on whether the `TableDelegate` API fits SoA data without per-row allocation | NFR-03/04/05/06 |
| S-2 | **Clipboard with custom MIME types.** Copy files in Duet-spike → paste in Nautilus and Dolphin. Copy in Nautilus/Dolphin → paste in Duet-spike. Both cut and copy semantics. | 3 d | Verdict on whether GPUI's clipboard API suffices; if not, a working prototype using `wl_data_device` (smithay-client-toolkit) or `x11rb` selections *alongside* a GPUI window, with the integration cost estimated in days | FR-CFG-05, R-G2, OQ-1 |
| S-3 | **Cross-application drag & drop.** Drag from Duet-spike to Nautilus/Dolphin/a terminal; drag from Firefox and Nautilus into Duet-spike. Wayland and X11. | 3 d | Verdict + prototype or a documented "defer to P1" recommendation with the reason | FR-CFG-06, R-G3, OQ-2 |
| S-4 | **Directory enumeration throughput.** Non-UI benchmark: enumerate and stat 100k and 1M entries on ext4/btrfs/tmpfs, comparing naive `read_dir`+`stat`, `d_type`-aware skipping, and parallel batched `statx`. | 2 d | A measured table of strategies; the winning strategy documented and the numbers recorded as the Phase 3 baseline | NFR-03/04 |
| S-5 | **Copy strategy ladder.** Micro-benchmark `FICLONE`, `copy_file_range`, buffered copy with and without `fadvise(DONTNEED)`, and `io_uring` batching, on large-file and many-small-file corpora, against `cp` and `cp -a`. | 3 d | Measured throughput table; go/no-go on `io_uring` for 1.0; page-cache eviction behaviour of each verified with `vmtouch` | FR-OPS-06, NFR-07 |
| S-6 | **Text input reality check.** Path bar and inline rename with CJK IME, dead keys, emoji, RTL, and clipboard paste of a 4000-char path. | 2 d | Verdict on GPUI's input handling; defects filed upstream if found | R-G8, NFR-11 |
| S-7 | **WASM plugin round trip.** Load a trivial `wasmtime` component implementing a content plugin returning one column value; measure per-call overhead. | 3 d | Per-call overhead under 100 µs warm; fuel/epoch interruption demonstrated to actually stop a `loop {}` plugin | FR-PLUG-01/06 |
| S-8 | **Packaging shape.** Build a hello-GPUI app as Flatpak and AppImage; run on a container with no GTK/Qt runtime. | 1 d | Both artifacts run on a bare Wayland session; binary size recorded | NFR-09/10 |
| T-0.9.1 | **Write the feasibility report.** Consolidate S-1…S-8 into a G0 decision memo: proceed / proceed-with-changes / choose Iced. | 2 d | Memo exists, ADR-001 updated, `design.md` §7.4 risk table updated with measured facts replacing guesses | INFRA |

**Phase 0 total: 21 d.**

**Kill criteria — be honest at this gate.** If S-1 shows the table cannot hold 120 Hz at 1M rows *and* the delegate API forces per-row allocation, the primary differentiator is gone and Iced or GTK4's `ColumnView` deserve a second look. If S-2 fails with no workaround under ~10 days, a file manager that cannot exchange files with other file managers is not shippable; either fund the protocol work explicitly or change frameworks.

---

# Phase 1 — Requirements baseline

**Objective:** freeze *what* before designing *how*. Waterfall's one real gift is that this phase makes the rest cheap.

**Entry criteria:** G0 passed.
**Exit criteria (gate G1):** `design.md` §4–§6 frozen; every requirement has a priority, an owner phase, and an acceptance test sketch; the keymap appendix is complete.

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-1.1.1 | Install and use TC 11 (under Wine), Double Commander, Krusader, and mc for a full working week each; keep a log of every interaction that felt right or wrong | 3 d | — | A written behavioural log; at least 20 concrete observations that change or confirm a requirement | All |
| T-1.2.1 | Requirement review pass: walk §6 line by line, assign P0/P1/P2, delete anything that cannot be justified against a persona | 2 d | T-1.1.1 | No requirement without a priority and a persona; scope-cut list recorded | All |
| T-1.3.1 | Write an acceptance-test sketch for every P0 requirement (one or two sentences: how would we know?) | 2 d | T-1.2.1 | 100% P0 coverage; sketches feed T-10.4.1 | All |
| T-1.4.1 | Extract Total Commander's complete default keymap into a machine-readable table, with each binding's exact TC behaviour | 2 d | T-1.1.1 | `docs/keymap-tc.csv` with ≥150 bindings, each verified by hand in TC | FR-CFG-02 |
| T-1.4.2 | Produce `design.md` Appendix A: the Duet default keymap, marking every deviation from TC with a written rationale | 2 d | T-1.4.1 | Every deviation has a defence; Linux-convention conflicts (Ctrl+C/V/X, Ctrl+W, F10) explicitly resolved | FR-CFG-02, FR-SEL-\* |
| T-1.5.1 | Command catalogue: enumerate every command the app will register, with id, title, category, and preconditions | 2 d | T-1.4.2 | `docs/commands.md` with ≥200 entries; every keymap binding resolves to a listed command | G-4, FR-TOOL-11 |
| T-1.6.1 | Config schema draft: `settings.toml`, `keymap.toml`, `connections.toml`, theme token list | 1 d | T-1.2.1 | Schema documented with defaults and value ranges | FR-CFG-01/04 |

**Phase 1 total: 14 d.**
**DoD:** requirements frozen. Changes after this point go through a change note appended to §6 with an impact estimate — that is the discipline that makes the estimates in this document mean anything.

---

# Phase 2 — Architecture & detailed design

**Entry criteria:** G1 passed.
**Exit criteria (gate G2):** all interfaces below compile as trait/type skeletons with `todo!()` bodies; ADRs written; no interface question left to implementation time.

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-2.1.1 | Create the workspace: crate skeletons per `design.md` §8.1, CI (build/clippy/fmt/test), MSRV pin, license headers | 2 d | — | `cargo build --workspace` green; CI runs on push; crate graph matches §8.1 | INFRA |
| T-2.1.2 | Dependency-graph lint: CI fails if any crate other than `duet-ui`/`duet-widgets` depends on `gpui` | 1 d | T-2.1.1 | A deliberate violation PR fails CI | ADR-002 |
| T-2.2.1 | Define `duet-types`: `VPath`, `MountId`, `EntryId`, `Metadata`, `Caps`, `MetaPatch`, error taxonomy | 3 d | T-2.1.1 | Types compile; `VPath` round-trips through display/parse under `proptest`; nesting (`zip:file://…!/…`) representable | FR-VFS-01/05/06 |
| T-2.2.2 | Define the `FileSystem` trait and `ListOpts`/`WriteOpts`/`AsyncWriteCommit` per §9.1 | 2 d | T-2.2.1 | Trait compiles; a `NullFs` stub implements it; doc comments state the contract for every method including error semantics | FR-VFS-01/06 |
| T-2.3.1 | Design the operation engine interfaces: `Plan`, `Step`, `Job`, `JobEvent`, `ConflictPolicy`, `Journal` | 3 d | T-2.2.2 | Types compile; a hand-written plan for "copy a 3-file directory" serialises and deserialises | FR-OPS-\* |
| T-2.3.2 | **Write the crash-safety proof sketch**: for every `Step` kind, enumerate the interruption points and state the invariant that holds at each | 2 d | T-2.3.1 | A table in `docs/crash-safety.md` covering every step × every interruption point; each row names the test that will verify it | FR-OPS-07, NFR-08 |
| T-2.4.1 | Design the command registry, keymap resolution, and context predicate evaluator | 2 d | T-1.5.1 | Interfaces compile; predicate grammar documented with a parser test corpus | G-4, FR-CFG-02 |
| T-2.5.1 | Design the panel model: `EntryStore` SoA layout, `DirectoryModel`, event/diff protocol to the UI | 2 d | T-2.2.1, S-1 | Memory layout documented with a per-entry byte budget (target ≤ 96 B + name); diff protocol covers insert/remove/update/reorder/reset | NFR-06 |
| T-2.6.1 | Write the WIT interface definitions for all five plugin classes | 2 d | S-7 | `wit-bindgen` generates host and guest bindings; a stub plugin compiles against them | FR-PLUG-02 |
| T-2.7.1 | ADR write-up: ADR-001…ADR-006 finalised, OQ-5 (`opendal` vs. hand-rolled) decided | 1 d | all above | Each ADR states context, options, decision, consequences | INFRA |

**Phase 2 total: 20 d.**

---

# Phase 3 — Core foundations

**Objective:** the parts that make the product fast and safe, all headless and testable without a window.

**Entry criteria:** G2 passed.

## 3.1 Local filesystem backend

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-3.1.1 | `LocalFs`: `read_dir` streaming with `getdents64`, `d_type` fast path, chunked emission | 4 d | T-2.2.2, S-4 | 100k entries enumerated (names + types only) in ≤ 60 ms on tmpfs; first chunk emitted in ≤ 5 ms | FR-VFS-01, NFR-03 |
| T-3.1.2 | Parallel batched `statx` with `AT_STATX_DONT_SYNC`, driven by `ListOpts` field requests | 3 d | T-3.1.1 | Full metadata for 100k entries in ≤ 250 ms; requesting fewer fields measurably reduces time | NFR-03 |
| T-3.1.3 | `*at`-relative traversal helpers (open dirfd, `openat`, `unlinkat`, `renameat2`, `mkdirat`, `fstatat`) wrapped over `rustix` | 3 d | T-2.2.1 | No path re-resolution in any traversal; a TOCTOU test (rename a directory mid-walk) does not escape the intended subtree | §13 |
| T-3.1.4 | Read/write handles: `AsyncReadSeek`, `AsyncWriteCommit` with temp-file + atomic rename | 3 d | T-3.1.3 | A killed write leaves the original intact and a `.duet-partial-*` file; verified by test | FR-OPS-07 |
| T-3.1.5 | Metadata get/set: mode, times, ownership, xattrs, POSIX ACLs, SELinux label | 4 d | T-3.1.3 | Round-trip test preserves all of the above on ext4 and btrfs; unsupported attributes degrade with a recorded warning, not an error | FR-OPS-05 |
| T-3.1.6 | UI-thread blocking guard: a thread-local flag set by the shell; every `LocalFs` syscall wrapper asserts it is unset in debug builds | 1 d | T-3.1.1 | A deliberate UI-thread call panics in debug; zero overhead in release (verified by disassembly or bench) | NFR-02 |
| T-3.1.7 | Filesystem-property probing: `st_dev`, rotational detection, reflink support, `statfs` type, case sensitivity | 2 d | T-3.1.3 | Correct results on ext4/btrfs/xfs/tmpfs/exfat/nfs; cached per mount | FR-OPS-06 |
| T-3.1.8 | VFS conformance suite v1, run against `LocalFs` | 4 d | T-3.1.1…5 | ≥ 60 test cases covering listing, metadata, rename, remove, symlinks, permissions, unicode names, very long names, and the capability-honesty checks | §14.2 |

## 3.2 Directory model & watching

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-3.2.1 | `EntryStore`: SoA storage with an interned name arena | 3 d | T-2.5.1 | 1M entries ≤ 120 MB; zero per-entry heap allocations for names under 24 bytes (verified with a counting allocator) | NFR-06 |
| T-3.2.2 | Sorting: per-column key precomputation, locale collation, natural-numeric mode, directories-first | 3 d | T-3.2.1 | 1M-entry sort ≤ 400 ms; stable; a unicode collation test corpus passes | FR-NAV-06 |
| T-3.2.3 | Filtering: hidden files, quick-filter substring, mask-based filters, saved filters | 2 d | T-3.2.1 | Filter over 1M entries ≤ 80 ms; filter and sort compose without a full rebuild | FR-NAV-07 |
| T-3.2.4 | Selection: `RoaringBitmap` keyed by stable `EntryId`, survives sort/filter/refresh | 2 d | T-3.2.1 | Selecting 500k entries and re-sorting preserves the exact set; statistics update in ≤ 5 ms | FR-SEL-01/04/05 |
| T-3.2.5 | Watching: `notify` integration, 50 ms debounce, event coalescing, `IN_Q_OVERFLOW` → rescan | 3 d | T-3.2.1 | Create/delete/rename/modify in an external terminal reflect within 100 ms; 10k rapid changes coalesce without a stall | FR-NAV-01 |
| T-3.2.6 | Polling fallback for backends without `WATCH`, with an adaptive interval | 2 d | T-3.2.5 | NFS/sshfs mounts refresh on an interval; CPU cost measured and bounded | FR-VFS-06 |
| T-3.2.7 | Diff protocol: produce minimal update batches for the UI from model mutations | 2 d | T-3.2.1 | A 1-entry change produces a 1-entry diff, not a reset; property test over random mutation sequences | NFR-05 |
| T-3.2.8 | Directory-size computation service: cancellable, cached by `(dev, ino, mtime)`, persisted | 3 d | T-3.1.1 | Sizing a 100k-file tree does not stall the UI; cache hit is ≤ 1 ms; invalidated by watch events | FR-SEL-02 |

## 3.3 Cross-cutting core

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-3.3.1 | Config loading: TOML with round-trip preservation, schema versioning, migration runner, hot reload | 4 d | T-1.6.1 | Editing `settings.toml` externally applies within 200 ms; an unknown key survives a rewrite; a v0→v1 migration test passes with a backup written | FR-CFG-01 |
| T-3.3.2 | Command registry + keymap parser + context predicate evaluator | 4 d | T-2.4.1 | 200 commands register; the TC keymap loads; binding conflicts produce diagnostics with file/line | FR-CFG-02, FR-TOOL-11 |
| T-3.3.3 | Logging/tracing setup, ring buffer, crash file writer | 2 d | T-2.1.1 | A forced panic writes a crash file containing the last 200 trace events and the session state | §12 |
| T-3.3.4 | Benchmark harness + corpus generator (10/1k/100k/1M entries, deep trees, unicode, sparse, hardlink farms, broken symlinks) | 4 d | T-2.1.1 | `cargo bench` produces a stable report; corpus generation is deterministic from a seed; baselines recorded in-repo | NFR-01…07 |
| T-3.3.5 | CI performance gate: fail on >10% regression against recorded baselines | 2 d | T-3.3.4 | A deliberate 20% regression PR fails CI | §11 |

**Phase 3 total: 66 d.**
**DoD:** a headless binary can enumerate, sort, filter, and select in a 1M-entry directory within NFR targets, with the conformance suite green — with no window on screen.

---

# Phase 4 — UI shell (walking skeleton, M1)

**Objective:** the first thing that looks like the product. Read-only: navigation and selection, no operations. This is where GPUI risk becomes concrete, so it comes before the expensive operations work.

**Entry criteria:** Phase 3 DoD.

## 4.1 Shell scaffolding

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-4.1.1 | GPUI application bootstrap: window, root view, theme init, `gpui-component` init, executor wiring to the core's Tokio runtime | 3 d | T-3.3.2 | A window opens on Wayland and X11; the core's async tasks drive UI updates through the foreground executor | ADR-001 |
| T-4.1.2 | `duet-widgets` façade: re-export and wrap every `gpui-component` widget used, so no other module imports it directly | 2 d | T-4.1.1 | CI lint forbids `gpui_component::` outside `duet-widgets`; the façade covers table, list, input, select, menu, dialog, toast, resizable panels | R-G7 |
| T-4.1.3 | `gpui-compat` shim module for churn-prone APIs; GPUI version pinned exactly | 1 d | T-4.1.1 | Pin recorded in `Cargo.lock` and documented; shim has a note per wrapped API explaining why | ADR-003 |
| T-4.1.4 | Workspace layout: splitter (draggable + keyboard-resizable), function-key bar, status bar, command-line row | 3 d | T-4.1.2 | Splitter ratio persists across restart; keyboard resize works; layout survives window resize and DPI change | FR-NAV-01 |
| T-4.1.5 | Theme system: token set, light/dark, follow-system detection, theme file loading | 3 d | T-4.1.1 | Switching the desktop colour scheme flips Duet without restart; a custom theme file loads and hot-reloads | FR-CFG-04 |

## 4.2 The file table

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-4.2.1 | `FileTable` view over `EntryStore`, virtualised, reading columns by index with no per-row allocation | 6 d | T-3.2.1, S-1 | 1M rows scroll at monitor refresh with no frame over 8.3 ms at 120 Hz; a counting allocator shows zero allocations per frame while scrolling | NFR-05 |
| T-4.2.2 | Cursor rendering, keyboard movement (arrows, Home/End, PgUp/PgDn, Ctrl+Home/End), scroll-into-view | 2 d | T-4.2.1 | Cursor movement is one frame; holding Down scrolls smoothly at key-repeat rate | FR-SEL-01 |
| T-4.2.3 | Selection rendering + all selection commands (Insert, Space, masks, invert, select-all, same-extension) | 3 d | T-3.2.4, T-4.2.2 | Every FR-SEL command is bound, works, and updates the footer statistics live | FR-SEL-02/03/05 |
| T-4.2.4 | Column configuration: add/remove/reorder/resize, persisted per view mode; header click-to-sort | 3 d | T-4.2.1 | Column layout survives restart; drag-resize is smooth; header sort indicator correct | FR-NAV-05/06 |
| T-4.2.5 | View modes: Full, Brief (multi-column), Thumbnails (placeholder icons for now), Tree | 5 d | T-4.2.1 | All four render correctly and switch without a visible reflow stall; per-tab persistence works | FR-NAV-04 |
| T-4.2.6 | Icon rendering: XDG icon-theme resolution, GPU atlas cache, per-extension mapping | 4 d | T-4.2.1 | Icons appear for common types; scrolling with icons stays inside NFR-05; atlas memory bounded and evicted | FR-TOOL-08 |
| T-4.2.7 | Panel footer and header: path display, free-space indicator, selection statistics, active-panel treatment | 2 d | T-4.2.1 | Free space updates on mount changes; the active panel is unmistakable at a glance | FR-NAV-02 |

## 4.3 Navigation

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-4.3.1 | Navigation commands: enter directory, parent, root, home, back/forward history per tab | 2 d | T-4.2.2 | Cursor restores to the child directory when going up — the detail that makes navigation feel right | FR-NAV-08 |
| T-4.3.2 | Tabs: create, close, reorder, lock, lock-with-dir-change, persist across restart | 4 d | T-4.3.1 | TC tab semantics reproduced; 20 tabs restore correctly after a restart | FR-NAV-03 |
| T-4.3.3 | Quick search (type-to-jump) and quick filter (type-to-filter) with a visible mode indicator | 3 d | T-3.2.3 | Typing in a 1M-entry panel jumps within one frame; filter mode shows the pattern and match count | FR-NAV-07 |
| T-4.3.4 | Editable path bar with completion, and breadcrumb segments | 3 d | T-4.3.1, S-6 | Tab-completion works for partial paths; IME input works; a 4000-char path pastes without breaking layout | FR-NAV-09 |
| T-4.3.5 | Directory hotlist (bookmarks) with keyboard overlay, add/remove/reorder | 2 d | T-3.3.1 | Ctrl+D opens, arrow+Enter navigates, entries persist | FR-NAV-08 |
| T-4.3.6 | Command palette: fuzzy search over all commands, showing bindings | 3 d | T-3.3.2 | Every registered command is reachable; fuzzy ranking is sensible; opening is instant with 200+ commands | FR-TOOL-11 |
| T-4.3.7 | Session persistence: panes, tabs, cwds, cursor, sort, view mode, splitter | 2 d | T-4.3.2 | Kill -9 then restart restores the full workspace; a corrupt session file degrades to defaults with a notice | FR-CFG-01 |
| T-4.3.8 | Mouse support: click, double-click, right-click context menu, both selection conventions, wheel, middle-click | 3 d | T-4.2.3 | Both Windows-style and Norton-style selection modes behave exactly as TC does | FR-SEL-06 |

**Phase 4 total: 58 d.**
**M1 exit demo:** open Duet, navigate two panels through a 1M-entry directory tree with tabs, sort, filter, select 100k files by mask, and watch external changes appear live — all at 120 fps, with nothing yet able to modify a file.

---

# Phase 5 — Operations engine & TC parity (Alpha, M2)

**Objective:** the part where mistakes cost users their data. Highest care, highest test density.

**Entry criteria:** M1 demo passed.

## 5.1 Engine core

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-5.1.1 | Planner: async cancellable source walk producing a materialised `Plan` with totals | 4 d | T-2.3.1, T-3.1.3 | Planning 100k files completes in ≤ 2 s and reports accurate totals; cancellation is immediate; a plan is serialisable | FR-OPS-02/03 |
| T-5.1.2 | Journal: append-only, fsync'd, with intent and completion records; recovery reader | 4 d | T-5.1.1, T-2.3.2 | A SIGKILL at any point leaves a readable journal; recovery reconstructs the exact remaining work; journal write overhead ≤ 3% of copy time | FR-OPS-07 |
| T-5.1.3 | Executor: step loop, worker pool, per-device concurrency, cancellation, pause/resume | 5 d | T-5.1.1 | Pause stops within 200 ms mid-file and resumes correctly; concurrency respects rotational detection | FR-OPS-02 |
| T-5.1.4 | Copy strategy ladder: FICLONE → copy_file_range → sparse-aware buffered with `fadvise` | 5 d | S-5, T-3.1.4 | ≥ 95% of `cp` for a 10 GB file; reflink used automatically on btrfs (verified with `filefrag`); page cache not evicted (verified with `vmtouch`) | FR-OPS-06, NFR-07 |
| T-5.1.5 | Move: same-device `renameat2`, cross-device copy→verify→unlink | 3 d | T-5.1.4 | Cross-device move never unlinks before the destination is fsync'd; verified by injection test | FR-OPS-01/07 |
| T-5.1.6 | Metadata application ordering (content → mode → xattr → ACL → label → times → owner) | 2 d | T-3.1.5, T-5.1.4 | `getfacl`/`getfattr`/`stat` comparison between source and destination is byte-identical for a corpus of tricky files | FR-OPS-05 |
| T-5.1.7 | Hardlink graph preservation within a job | 2 d | T-5.1.4 | Copying an rsnapshot-style tree preserves link counts; memory for the inode map is bounded and reported | FR-OPS-05 |
| T-5.1.8 | Delete: recursive, symlink-safe, trash and permanent paths | 3 d | T-3.1.3 | Recursive delete never follows a symlink out of the tree (explicit test with a symlink to `/`); read-only files prompt rather than silently failing | FR-OPS-01, §13 |
| T-5.1.9 | Conflict resolution engine: policy resolution, per-conflict prompt data, apply-to-all | 3 d | T-5.1.3 | All seven TC policies implemented; property test over random conflict sequences shows no policy leaks between jobs | FR-OPS-04 |
| T-5.1.10 | Error taxonomy handling: retry with backoff, ENOSPC pauses the queue, EACCES offers elevation | 3 d | T-5.1.3 | Injected transient errors retry and succeed; ENOSPC pauses all jobs and surfaces one clear message | §9.3 |
| T-5.1.11 | Progress and ETA: atomic counters, 100 ms sampling, dual-regime EMA | 2 d | T-5.1.3 | ETA on a mixed corpus is within 20% after the first 10 s and never runs backwards for more than one sample | FR-OPS-03 |
| T-5.1.12 | Post-copy verification (BLAKE3) as a job flag | 2 d | T-5.1.4 | Verification detects a deliberately corrupted destination; throughput cost measured and documented | FR-OPS-08 |
| T-5.1.13 | Queue manager: job ordering, priorities, reordering, aggregate state | 3 d | T-5.1.3 | 10 queued jobs behave predictably; reordering a running job is either supported or cleanly refused | FR-OPS-02 |

## 5.2 Operations UI

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-5.2.1 | Copy/move dialog (F5/F6): destination with completion, options, queue-vs-run choice | 3 d | T-5.1.1 | Keyboard-complete; TC-familiar layout; destination defaults to the target panel | FR-OPS-01 |
| T-5.2.2 | Progress UI: status-bar tray + expandable operation manager with per-job controls | 4 d | T-5.1.11 | Progress redraws do not cost more than 0.5 ms/frame; the tray is unobtrusive but the manager is one key away | FR-OPS-02/03 |
| T-5.2.3 | Conflict dialog with side-by-side metadata, on-demand hashes, and all policy buttons | 3 d | T-5.1.9 | Keyboard-complete; a 10k-conflict run is survivable using apply-to-all | FR-OPS-04 |
| T-5.2.4 | Error/skip report view with re-run-failed | 2 d | T-5.1.10 | A job with 50 permission errors ends with an actionable list, not 50 dialogs | §12 |
| T-5.2.5 | Interrupted-operation recovery UI at startup | 2 d | T-5.1.2 | After a kill mid-copy, the next launch offers resume/discard/inspect and all three work | FR-OPS-07 |
| T-5.2.6 | Delete confirmation with the correct defaults (trash vs permanent, non-empty directory warning) | 2 d | T-5.1.8 | Matches the configured policy; Shift+Del bypasses trash with an explicit confirmation | FR-OPS-01 |
| T-5.2.7 | Create directory (F7), in-place rename (Shift+F6), create symlink/hardlink dialogs | 3 d | T-5.1.1 | F7 supports creating nested paths in one go, as TC does; inline rename selects the stem, not the extension | FR-OPS-01 |
| T-5.2.8 | Attributes/permissions dialog with recursive apply and timestamp editing | 3 d | T-3.1.5 | Octal and symbolic entry agree; recursive apply runs through the operation queue, not synchronously | FR-OPS-12 |

## 5.3 Essential integration

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-5.3.1 | Trash: full freedesktop spec implementation incl. `$topdir/.Trash-$uid` | 4 d | T-3.1.3 | Trashing on a second mount works; `.trashinfo` is spec-correct; GNOME Files and Dolphin both show and restore Duet-trashed files | FR-CFG-07 |
| T-5.3.2 | Trash browser view with restore and empty | 2 d | T-5.3.1 | Restore returns files to their original paths; restore into a deleted parent recreates it or reports clearly | FR-CFG-07 |
| T-5.3.3 | Clipboard: copy/cut/paste files with `text/uri-list` + GNOME/KDE markers (path chosen by S-2) | 5 d | S-2 | Bidirectional copy and cut verified against Nautilus, Dolphin, Thunar, and PCManFM | FR-CFG-05 |
| T-5.3.4 | MIME detection + association launching (desktop entries, field codes, terminal apps) | 4 d | T-3.3.1 | Enter on a `.pdf`, `.png`, `.txt`, and a shell script each do the right thing; "Open With" lists real apps | FR-TOOL-08 |
| T-5.3.5 | Command line at the bottom: shell execution with cwd, history, completion, insert-name/path keys | 4 d | T-4.1.4 | Running `make` from the panel works; Ctrl+Enter inserts the filename; history persists | FR-TOOL-06 |
| T-5.3.6 | Internal viewer (F3): text with encoding detection, hex, image, incremental line index, in-viewer search | 8 d | T-3.1.4 | A 5 GB log opens in ≤ 300 ms and scrolls to the end instantly; a minified 200 MB single-line JSON does not hang; encoding override works | FR-TOOL-01 |
| T-5.3.7 | Search (Alt+F7): masks, regex, size/date filters, content search, streaming results, **feed to panel** | 8 d | T-3.1.1 | Content search of a 5 GB tree streams first results in ≤ 500 ms; feed-to-panel produces a fully operable synthetic listing | FR-TOOL-04 |
| T-5.3.8 | Quick view panel (Ctrl+Q) reusing the viewer stack | 2 d | T-5.3.6 | Cursor movement updates the preview without stutter; large files preview lazily | FR-TOOL-02 |

**Phase 5 total: 104 d.**
**M2 / Gate G3 exit criteria:** all FR-OPS P0 and FR-NAV/SEL P0 complete; the data-safety suite (T-10.2.1, brought forward and run continuously from T-5.1.2 onward) passes 100%; a full day of dogfooding as the author's only file manager produces no data loss and no crash.

---

# Phase 6 — Archives & VFS expansion

**Entry criteria:** G3 passed.

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-6.1.1 | VFS mount table: nesting, reference counting, lifecycle, path display and parsing for nested mounts | 4 d | T-2.2.1 | `zip:` on `sftp:` resolves; a mount is torn down when its last user closes; nesting depth capped with a clear error | FR-VFS-01/05 |
| T-6.1.2 | Archive backend framework: probe, list, extract-stream, and the read-only capability profile | 4 d | T-6.1.1 | The conformance suite (T-3.1.8) runs against archive backends and passes the read-only subset | FR-VFS-02/06 |
| T-6.1.3 | Zip backend (read + write), incl. zip64, encryption detection, and correct filename-encoding handling | 4 d | T-6.1.2 | A 100k-member zip lists in ≤ 500 ms; CP437/UTF-8 names resolve correctly; zip-slip members rejected | FR-VFS-02/03 |
| T-6.1.4 | Tar family (tar, gz, bz2, xz, zstd) with a seek index for compressed streams | 5 d | T-6.1.2 | Random access into a 2 GB `.tar.zst` does not re-decompress from the start after the index is built; index build is backgrounded | FR-VFS-02 |
| T-6.1.5 | 7z (read + write) and rar (read) backends | 4 d | T-6.1.2 | Solid archives handled without O(n²) extraction; encrypted archives prompt for a password once per session | FR-VFS-02 |
| T-6.1.6 | Container formats: iso, deb, rpm, cab, ar | 3 d | T-6.1.2 | Each lists and extracts; a `.deb` shows its control and data members sensibly | FR-VFS-02 |
| T-6.1.7 | Archive security hardening: path traversal, symlink members, ratio bombs, member-count limits | 3 d | T-6.1.3…6 | A curated malicious-archive corpus is fully rejected with clear messages; fuzzing runs clean for 24 h | §13 |
| T-6.1.8 | Pack dialog (Alt+F5) with format, compression level, split-volume, password, and "move to archive" | 4 d | T-6.1.3 | Packing runs through the operation queue with real progress; options persist as defaults | FR-VFS-03 |
| T-6.1.9 | Unpack dialog (Alt+F9) with destination, path handling, and overwrite policy | 2 d | T-6.1.2 | Unpacking uses the same conflict machinery as copy; no separate code path | FR-VFS-03 |
| T-6.1.10 | Operation engine adaptation to non-POSIX backends (capability-driven strategy selection) | 5 d | T-6.1.2, T-5.1.3 | Copying *into* an archive and *out of* one both work through the normal F5 flow; capabilities determine the strategy with no backend-specific branches in the executor | FR-VFS-06 |
| T-6.1.11 | Archive-aware search (search inside archives) | 3 d | T-5.3.7, T-6.1.2 | Content search descends into archives when enabled, with a clear cost warning | FR-TOOL-04 |
| T-6.1.12 | Extend the conformance suite to cover write-capable backends and capability honesty | 3 d | T-6.1.3 | Any backend claiming a capability it does not have fails the suite | §14.2 |
| T-6.1.13 | Branch view (Ctrl+B) built on the traversal layer | 2 d | T-3.1.1 | Flat view of a 200k-file tree streams in and remains fully operable | FR-NAV-10 |
| T-6.1.14 | Mount/drive bar: udisks2 integration, mount/unmount/eject, GVFS mount discovery | 6 d | T-3.1.7 | Plugging in a USB stick shows it within 1 s; unmount reports busy conditions usefully; GVFS mounts appear | FR-NAV-11, FR-VFS-07 |

**Phase 6 total: 52 d.**

---

# Phase 7 — Remote backends

**Entry criteria:** Phase 6 DoD. Decision from OQ-5 applied.

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-7.1.1 | Remote backend framework: connection pooling, reconnection, timeouts, latency-aware `ListOpts` batching, capability profiles | 5 d | T-6.1.1 | A 200 ms-latency link lists a 1000-entry directory in ≤ 2 s; connection loss surfaces as retryable, not fatal | FR-VFS-04/06 |
| T-7.1.2 | Credential storage: Secret Service keyring, memory zeroisation, per-profile secrets | 3 d | T-3.3.1 | No secret ever written to a config file; a keyring-less system degrades to session-only prompts | §13 |
| T-7.1.3 | SFTP backend | 6 d | T-7.1.1 | Conformance suite passes; strict host-key checking with a TOFU prompt; resume of an interrupted upload works | FR-VFS-04 |
| T-7.1.4 | FTP/FTPS backend | 4 d | T-7.1.1 | Passive/active modes, TLS, and the reduced-durability warning when atomic replace is unavailable | FR-VFS-04 |
| T-7.1.5 | WebDAV backend | 3 d | T-7.1.1 | Conformance suite passes against at least two server implementations | FR-VFS-04 |
| T-7.1.6 | S3-compatible backend (multipart upload, prefix-as-directory semantics) | 5 d | T-7.1.1 | 5 GB upload via multipart with resumability; "directories" behave sanely; costs of `LIST` made visible | FR-VFS-04 |
| T-7.1.7 | SMB backend | 5 d | T-7.1.1 | Conformance suite passes against Samba; Kerberos and NTLM both tested | FR-VFS-04 |
| T-7.1.8 | Connection manager UI: profiles, test-connection, import from `~/.ssh/config` | 4 d | T-7.1.2 | Creating an SFTP profile from an existing SSH host takes under 15 seconds of user time | FR-VFS-04 |
| T-7.1.9 | Remote-aware operation UX: bandwidth display, per-connection concurrency, resume-on-reconnect | 4 d | T-5.1.3, T-7.1.1 | Pulling the network cable mid-transfer pauses and resumes cleanly on reconnect | FR-OPS-02, FR-VFS-06 |
| T-7.1.10 | Fault-injection harness for remote backends (latency, packet loss, mid-stream disconnect, truncated responses) | 4 d | T-7.1.1 | Every remote backend runs the full fault matrix in CI with no data loss and no hang | §14.4 |
| T-7.1.11 | Extend conformance + capability-honesty coverage to all remote backends | 3 d | T-7.1.3…7 | All backends green | §14.2 |

**Phase 7 total: 46 d.**

---

# Phase 8 — Plugin system

**Entry criteria:** Phase 7 DoD. Re-confirm this phase is wanted — see `design.md` §16.1.

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-8.1.1 | Wasmtime host: engine setup, component instantiation, per-instance memory caps, fuel/epoch interruption | 5 d | S-7, T-2.6.1 | A `loop {}` plugin is killed within 2 s without affecting the app; memory cap enforced and reported | FR-PLUG-01/06 |
| T-8.1.2 | Capability enforcement: manifest parsing, grant model, handle-based file access | 5 d | T-8.1.1 | A plugin granted `*.jpg` cannot open `~/.ssh/id_rsa` — proven by a red-team test plugin that tries every escape it can | FR-PLUG-03, §13 |
| T-8.1.3 | Plugin lifecycle: discovery, load, unload, hot-reload for `--dev-plugin`, panic isolation and restart | 4 d | T-8.1.1 | A panicking plugin marks its feature degraded and the app continues; dev-mode reload picks up a rebuilt wasm within 2 s | FR-PLUG-06 |
| T-8.1.4 | Content plugin class: dynamic columns, sort keys, search fields, multi-rename placeholders | 6 d | T-8.1.2, T-4.2.4 | An EXIF plugin adds a sortable "Camera" column to a 10k-photo directory; column values are computed lazily and cached; scrolling stays inside NFR-05 | FR-PLUG-02 |
| T-8.1.5 | Packer plugin class: register an archive format visible to the VFS | 6 d | T-8.1.2, T-6.1.2 | A plugin-provided format is browsable, extractable, and packable through the normal UI with no special-casing | FR-PLUG-02 |
| T-8.1.6 | Filesystem plugin class: register a VFS backend | 7 d | T-8.1.2, T-7.1.1 | A plugin-provided backend passes the conformance suite; progress and cancellation propagate across the WASM boundary | FR-PLUG-02 |
| T-8.1.7 | Viewer plugin class | 4 d | T-8.1.2, T-5.3.6 | A plugin renders a custom preview; rendering is sandboxed and time-limited | FR-PLUG-02 |
| T-8.1.8 | Command plugin class: register commands, menu items, and keybindings | 3 d | T-8.1.2, T-3.3.2 | Plugin commands appear in the palette and are bindable like built-ins | FR-PLUG-02 |
| T-8.1.9 | Plugin SDK crate + `cargo generate` template + reference plugins (EXIF columns, a toy archive format, a toy VFS) | 6 d | T-8.1.4…8 | A "custom column" plugin is ≤ 50 lines end to end; the template builds to wasm in one command | FR-PLUG-05 |
| T-8.1.10 | Plugin registry: index format, client, install/update/remove, signature verification | 6 d | T-8.1.3 | Installing a plugin from the index takes one command and one confirmation showing the requested capabilities | FR-PLUG-04 |
| T-8.1.11 | Plugin manager UI: browse, install, configure, permissions review, disable | 4 d | T-8.1.10 | Capabilities are shown in plain language before install; revoking a capability takes effect on reload | FR-PLUG-03 |
| T-8.1.12 | Plugin author documentation: guide, API reference, security model, publishing walkthrough | 2 d | T-8.1.9 | A third party can ship a working plugin using only the docs — validated by asking someone to try | FR-PLUG-05 |

**Phase 8 total: 58 d.**

---

# Phase 9 — Tools, polish, i18n, accessibility

**Entry criteria:** Phase 8 DoD (or Phase 6, on the reduced-scope path).

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-9.1.1 | Multi-rename tool: pattern language, counters, metadata and plugin placeholders, regex, case conversion, live preview | 8 d | T-8.1.4 | Renaming 5000 files with a pattern and a counter previews instantly and executes through the queue; collisions detected before execution | FR-OPS-09 |
| T-9.1.2 | Undo for the last rename batch, and for reversible operations generally | 4 d | T-9.1.1, T-5.1.2 | Undo restores exact original names; irreversible operations are visibly excluded from the undo stack | FR-OPS-09/14 |
| T-9.1.3 | Directory comparison: by name/size/date/content, with a diff-style presentation | 5 d | T-3.1.1 | Comparing two 100k-file trees by content completes with visible progress and correct results | FR-OPS-10 |
| T-9.1.4 | Synchronise directories: plan generation, direction selection, filters, execution through the queue | 6 d | T-9.1.3, T-5.1.13 | A dry-run plan is reviewable before execution and matches what actually happens, file for file | FR-OPS-10 |
| T-9.1.5 | Thumbnails: freedesktop cache, internal decoders, external `.thumbnailer` subprocess runner with sandbox+timeout | 6 d | T-4.2.5 | A 5000-image directory populates progressively without a frame drop; a malicious image cannot hang or crash the app; the cache is shared with other file managers | FR-TOOL-09 |
| T-9.1.6 | Properties dialog: metadata, permissions, ownership, xattrs, on-demand checksums, plugin fields | 4 d | T-3.1.5 | Multi-file selection shows aggregates; recursive size calculation is backgrounded | FR-TOOL-10 |
| T-9.1.7 | Internal editor (F4) over the `gpui-component` code editor, saving through `AsyncWriteCommit` | 6 d | T-5.3.6 | Editing a file on SFTP saves atomically where supported and warns where not; encoding and line endings preserved by default | FR-TOOL-03 |
| T-9.1.8 | File splitting/merging and checksum files (SFV, MD5, SHA-1, SHA-256, BLAKE3) | 4 d | T-5.1.4 | Split then merge reproduces a byte-identical file with checksum verification | FR-OPS-11 |
| T-9.1.9 | Directory tree panel synchronised with the active panel | 4 d | T-4.2.5 | Expanding a 50k-subdirectory tree stays responsive; selection syncs both ways | FR-NAV-12 |
| T-9.1.10 | Button bar: user-editable toolbar invoking commands, programs, and submenus | 4 d | T-3.3.2 | A user can add a button running a shell command against the selection, without editing files by hand | FR-TOOL-12 |
| T-9.1.11 | Embedded terminal panel (`alacritty_terminal`) sharing the panel cwd | 6 d | T-5.3.5 | Toggling the terminal is instant; cwd tracks the active panel when enabled; shell integration documented | FR-TOOL-07 |
| T-9.1.12 | Drag & drop implementation per the S-3 verdict (intra-app plus cross-app if feasible) | 6 d | S-3 | Dragging files to Nautilus and to a terminal both work on Wayland and X11, or the limitation is documented and the feature flagged | FR-CFG-06 |
| T-9.1.13 | Elevated operations: polkit action, D-Bus helper with a fixed verb set and argument validation | 5 d | T-5.1.10 | Copying into `/usr/local` prompts once and succeeds; the helper refuses malformed or traversing paths; a security review note is written | FR-OPS-13, §13 |
| T-9.1.14 | i18n: Fluent integration, string extraction, English catalogue complete, RTL layout check | 4 d | — | No user-visible string is hard-coded (CI lint); a pseudo-locale run reveals no truncation | FR-CFG-10 |
| T-9.1.15 | Accessibility pass: keyboard completeness audit of every dialog and view; focus order; a documented AT-SPI plan | 3 d | all UI | A scripted keyboard-only session performs every P0 workflow; the screen-reader gap is written up with an AccessKit assessment | NFR-11, OQ-4 |
| T-9.1.16 | `wincmd.ini` import tool | 3 d | T-3.3.1 | Colours, hotlist, and associations import from a real TC installation; unmapped settings are listed rather than silently dropped | FR-CFG-03 |
| T-9.1.17 | Settings UI editing the same TOML files, with live preview and a "show me the file" affordance | 5 d | T-3.3.1 | Changing a setting in the UI produces a minimal, comment-preserving diff in the file | FR-CFG-01 |

**Phase 9 total: 74 d.**

---

# Phase 10 — Hardening & QA (RC, M4)

**Entry criteria:** feature-complete for the targeted scope.

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-10.1.1 | Full VFS conformance run across every backend, including capability honesty | 3 d | all VFS | 100% pass; any exception documented as a known limitation in the release notes | §14.2 |
| T-10.2.1 | **Data-safety suite**: SIGKILL injection at every step boundary; ENOSPC via loop device; EACCES; `dm-flakey` I/O errors; mid-transfer disconnect | 10 d | T-2.3.2, T-5.1.2 | Every injection point satisfies the §9.3 invariant. **Release gate: 100%, no exceptions, no "flaky" annotations.** | NFR-08 |
| T-10.2.2 | Fuzzing campaign: archive parsers, config parser, encoding detection, plugin WIT boundary, path parsing | 5 d | T-6.1.7 | 72 h clean on each target; all discovered crashes fixed and added as regression corpora | §14.7 |
| T-10.2.3 | Platform matrix pass: GNOME/KDE/sway/XFCE, Wayland and X11, HiDPI 1×/1.5×/2×, mixed-DPI multi-monitor | 6 d | all UI | A defect log with every issue triaged; no P0 rendering or input defects remain | §14.5, R-G6 |
| T-10.2.4 | Filesystem matrix pass: ext4, btrfs, xfs, tmpfs, exFAT, NTFS3, NFS, SMB, sshfs, FUSE | 5 d | T-5.1.6 | Metadata preservation and operation correctness verified per filesystem; unsupported combinations degrade with a warning, never silently | FR-OPS-05 |
| T-10.3.1 | Performance validation against every NFR target, on both a fast and a deliberately slow machine | 4 d | T-3.3.4 | A results table in `docs/performance.md`; any miss either fixed or renegotiated in writing | NFR-01…07 |
| T-10.3.2 | Memory and leak audit: long-running session (72 h), 10k directory navigations, 1000 archive mounts | 3 d | — | RSS stable within 10% over 72 h; no mount, FD, or thread leaks (verified with `/proc/self/fd` and `valgrind`/`heaptrack`) | NFR-06 |
| T-10.3.3 | Startup profiling and optimisation to NFR-01 | 3 d | T-4.1.1 | ≤ 150 ms cold to interactive; the startup critical path documented | NFR-01 |
| T-10.4.1 | Execute the full acceptance-test pass from T-1.3.1 sketches | 5 d | T-1.3.1 | Every P0 requirement demonstrably passes; results recorded per requirement ID | All P0 |
| T-10.4.2 | External usability test with 3–5 real TC users (P1 persona), observed, not surveyed | 4 d | T-10.4.1 | ≥ 10 friction points logged; every "the keys are wrong" report resolved as fix-or-documented-deviation | G-2 |
| T-10.4.3 | Security review of the plugin sandbox, the polkit helper, and archive extraction, ideally by a second pair of eyes | 4 d | T-8.1.2, T-9.1.13 | Written review; all high findings fixed before release | §13 |
| T-10.5.1 | Bug-fix buffer for everything the above finds | 4 d | — | RC defect list at zero P0/P1 | — |

**Phase 10 total: 56 d.**

---

# Phase 11 — Packaging & release (1.0, M5)

| ID | Task | Est | Deps | AC | Traces |
|---|---|---|---|---|---|
| T-11.1.1 | Flatpak manifest, permission posture decided per OQ-6, published to Flathub | 5 d | S-8 | Installs and runs from Flathub; the filesystem-permission choice is explained in the app description honestly | §15 |
| T-11.1.2 | AppImage, `.deb`, `.rpm`, AUR `PKGBUILD`, tarball | 5 d | S-8 | Each installs cleanly on its target distro in a container test | §15 |
| T-11.1.3 | Release engineering: versioning, signing, SBOM, changelog generation, release CI | 3 d | T-11.1.2 | A tagged commit produces every artifact, signed, with an SBOM, unattended | §15 |
| T-11.1.4 | Desktop integration files: `.desktop`, AppStream metainfo, icons at all sizes, MIME registration, `FileManager1` D-Bus service | 3 d | T-5.3.4 | Duet is offerable as the default file manager; "Show in folder" from other apps opens Duet | FR-CFG-08 |
| T-11.1.5 | CLI interface and man page | 2 d | — | `--left/--right/--new-tab/--goto/--new-instance` all work; `man duet` is accurate | FR-CFG-09 |
| T-11.2.1 | User manual: getting started, the OFM model for newcomers, full keymap reference, configuration reference | 6 d | T-1.4.2 | A TC user finds their keys in under 30 s; a newcomer understands source/target panels from the intro | — |
| T-11.2.2 | Website with docs, screenshots, and downloads | 3 d | T-11.2.1 | Live; docs build from the repo on every release | — |
| T-11.2.3 | Release notes, known limitations (state the accessibility and DnD gaps plainly), announcement posts | 2 d | T-10.4.1 | Limitations are stated in the announcement, not buried — this buys credibility that is expensive to earn later | — |
| T-11.3.1 | Post-release support setup: issue templates, triage policy, crash-report intake (opt-in), a contribution guide | 1 d | — | Templates live; a written triage SLA even if it is "best effort, weekly" | — |

**Phase 11 total: 30 d.**

---

# Phase 12 — Post-1.0 backlog (not estimated)

| ID | Item | Traces |
|---|---|---|
| B-1 | AT-SPI accessibility via AccessKit, upstreamed into GPUI if viable | NFR-11, OQ-4 |
| B-2 | Native/Wine plugin bridge for real TC `.wcx`/`.wdx` | OQ-7 |
| B-3 | Windows and macOS builds (the core is already portable; `duet-platform` is not) | §2.2 |
| B-4 | MTP/PTP direct support without GVFS | FR-VFS-07 |
| B-5 | Saved searches as persistent filters | FR-TOOL-05 |
| B-6 | Nested VFS beyond two levels, with a better path UX | FR-VFS-05 |
| B-7 | Sync profiles with scheduling | FR-OPS-10 |
| B-8 | Colour-coding of files by rule (TC's colour filters) | — |
| B-9 | Panel-level scripting hooks (pre/post-operation) | — |
| B-10 | Tab groups / workspaces, and a session manager | — |

---

# Appendix — Critical path and sequencing notes

**Critical path:** S-1/S-2 → T-2.2.x → T-3.1.x → T-3.2.x → T-4.2.1 → T-5.1.x → T-5.2.x → T-10.2.1 → release. Everything else has slack. If schedule pressure appears, protect this chain and defer Phases 7 and 8 whole rather than thinning Phase 5 — a file manager that is 90% safe is worse than one that does less.

**Three sequencing decisions worth defending:**

1. **UI before operations (Phase 4 before Phase 5).** Inverts the usual "core first" instinct, deliberately: the GPUI bet is unproven until a 1M-row table is on screen, and discovering a framework problem after building the operations engine would waste far more than building the shell first does.
2. **The data-safety suite is written during Phase 5, not Phase 10.** T-10.2.1 is where it is *completed and gated*, but the harness is built alongside T-5.1.2 and run continuously. A crash-safety suite written after the fact tests what the code does, not what it should do.
3. **The plugin system comes last among the feature phases.** It is the largest single body of work with the least certain payoff, and every day spent on it before there are users is a day spent guessing at what extension authors want. `design.md` §16.1 recommends deferring it past 1.0 entirely for a solo effort.

**Weekly rhythm suggestion** (waterfall on paper still needs a heartbeat): one hour a week reviewing the current phase's DoD, updating actuals against estimates in this file, and recording any requirement change as a dated note in `design.md` §6. Estimate accuracy compounds — after three phases of recorded actuals, the remaining numbers stop being fiction.
