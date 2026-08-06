# Duet — Design Document

**A GPU-accelerated, keyboard-first orthodox (dual-pane) file manager for Linux**

| Field | Value |
|---|---|
| Document ID | DUET-DD-001 |
| Version | 0.1 (Draft) |
| Status | For review |
| Author | *(you)* |
| Date | 2026-08-02 |
| Supersedes | — |
| Related | `task.md` (DUET-WBS-001) |

### Revision history

| Ver | Date | Author | Change |
|---|---|---|---|
| 0.1 | 2026-08-02 | — | Initial draft |

### Approval gates (waterfall)

| Gate | Artifact | Exit criterion | Owner |
|---|---|---|---|
| G0 | Feasibility report | All Phase 0 spikes green or mitigated | Author |
| G1 | This document, §4–§6 frozen | Requirements baselined, IDs assigned | Author |
| G2 | This document, §7–§13 frozen | Architecture + interfaces baselined | Author |
| G3 | Alpha build | FR-CORE-\* complete, data-loss suite passing | Author |
| G4 | Beta build | FR-VFS-\*, FR-PLUG-\* complete | Author |
| G5 | 1.0 release | All P0/P1 requirements, packaging done | Author |

> **Note on codename.** "Duet" is a working title (dual pane, two voices in sync). Binary name `duet`, config dir `~/.config/duet`. Rename before G5 if a better name lands; §16.4 tracks the naming task.

---

## 1. Purpose

This document specifies the design of **Duet**, a native Linux file manager in the *orthodox file manager* (OFM) tradition established by Norton Commander and refined by Total Commander (TC). It is written waterfall-style: requirements are baselined before architecture, architecture before implementation, and each is traceable to the work breakdown in `task.md`.

The animating observation is this: on Linux, the OFM niche is served by Krusader (KDE/Qt, feature-rich, heavy), Double Commander (Free Pascal/Lazarus, the closest TC clone, but visibly slow on large directories and dated in rendering), Midnight Commander (TUI, excellent, but a TUI), and split-view modes bolted onto Dolphin/Nemo (not real OFMs — no command-line integration, no selection model, no operation queue). None of them feel like Zed feels. That gap — *TC's interaction model at Zed's latency* — is the product thesis.

## 2. Scope

### 2.1 In scope

Local file management, dual-pane UI with tabs, the full TC keyboard model, a robust background file-operation engine, a virtual filesystem layer (archives + remote protocols), a viewer and light editor, directory comparison and synchronisation, a multi-rename tool, a search engine with content matching, and a sandboxed WASM plugin system covering the four Total Commander plugin classes.

### 2.2 Out of scope (1.0)

- Windows and macOS builds. The architecture must not *preclude* them (§7.5), but no platform work is funded before 1.0.
- Binary compatibility with Total Commander's native `.wcx`/`.wfx`/`.wdx`/`.wlx` DLLs (Wine-hosted plugin loading). Tracked as a post-1.0 stretch (§17, OQ-7).
- A full text editor. F4 opens a *light* editor; heavier editing delegates to `$EDITOR` or Zed.
- Cloud-sync semantics (conflict-free replication, offline queues). Remote backends are session-scoped, not synced.
- Mobile/touch UI.

## 3. Stakeholders and personas

| Persona | Description | Primary need |
|---|---|---|
| **P1 — The TC refugee** | Used TC on Windows for 15 years, now on Linux. Muscle memory is in their fingers. | Keys must match TC *exactly*. A wrong default keybinding is a bug, not a preference. |
| **P2 — The power sysadmin** | Moves hundreds of GB between disks, over SSH, into archives. | Operation queue that never loses data, never lies about progress, resumes after failure. |
| **P3 — The developer** | Lives in a terminal and Zed; wants a file manager that doesn't break flow. | Sub-frame latency, command line at the bottom, embedded terminal, hackable config in plain text. |
| **P4 — The extension author** | Wants a custom column, a custom archive format, a custom viewer. | Documented, stable, safe plugin API with a real SDK and a registry. |

P1 is the primary persona. When P1 and P3 conflict, P1 wins on defaults and P3 wins on configurability.

## 4. Goals and non-goals

### 4.1 Goals

- **G-1 Latency as a feature.** Keystroke-to-pixel under one frame at 120 Hz. Directory changes visible before the user notices the transition.
- **G-2 TC-faithful interaction.** Selection model, function-key bar, command line, panel semantics, and default keymap match TC 11 behaviour unless a Linux convention makes that actively wrong.
- **G-3 Data safety is non-negotiable.** No file operation may lose or silently corrupt data, including under power failure, ENOSPC, network disconnect, or SIGKILL. This is a release gate, not a quality goal.
- **G-4 Everything is a command.** Every action is a named command; the keymap is a pure mapping from chords to commands; a command palette exposes all of them.
- **G-5 The core is UI-agnostic.** All non-rendering logic lives in crates that do not depend on GPUI, so the shell is replaceable (§7.4 risk mitigation).
- **G-6 Extensibility without ambient authority.** Plugins are WASM components with explicitly granted capabilities. A malicious plugin cannot read `~/.ssh`.

### 4.2 Non-goals

- Feature parity with GNOME Files' desktop integration surface (desktop icons, Nautilus scripts, GNOME search provider).
- Being a general Qt/GTK citizen. Duet renders itself; it will honour system fonts, cursor themes, icon themes, and dark-mode preference, but it does not use platform widgets.
- Mouse-first workflows. The mouse must work correctly and completely, but the design optimises for hands-on-keyboard.

## 5. Prior art and competitive analysis

| Product | Stack | Strengths | Weaknesses Duet exploits |
|---|---|---|---|
| Total Commander 11 | Win32, Delphi | The reference interaction model; 30 years of plugin ecosystem | Windows only; Wine-hosted TC is a second-class citizen on Linux |
| Double Commander | Free Pascal / Lazarus (GTK2/Qt5) | Closest TC clone on Linux, cross-platform, real plugin support (native WCX/WFX) | Sluggish on 100k+ entry directories; non-virtualised drawing; dated rendering; HiDPI/Wayland rough edges |
| Krusader | C++ / Qt / KDE Frameworks | Very complete; KIO gives it enormous protocol reach for free | Pulls in a large KDE dependency graph; feels heavy outside Plasma; KIO is both its superpower and its coupling |
| Midnight Commander | C, ncurses | Rock solid, ubiquitous, keyboard perfection | TUI-only: no thumbnails, no smooth scrolling, no rich viewer |
| Yazi / lf / nnn | Rust / Go / C, TUI | Modern, fast, async | TUI; not orthodox in the TC sense (single-pane-plus-preview) |
| Dolphin / Nemo split view | Qt / GTK | Beautiful, integrated | Split view is a viewport trick, not an OFM: no operation queue UI, no command line, no selection-by-mask, no F-key model |

**Conclusion:** the differentiated position is *Double Commander's semantics with Zed's rendering and a modern sandboxed plugin story, without a desktop-environment dependency*. §6 requirements are written to hit exactly that.

Also worth an explicit answer, because reviewers will ask: **why not contribute to Double Commander instead?** Because the performance ceiling there is set by the Lazarus widget-set (non-virtualised list drawing, synchronous directory enumeration on the UI thread) and by a Pascal codebase whose contributor pool is small. Fixing that is a rewrite in a language most contributors don't use. If the goal were "more features", contributing wins; the goal is "different feel", which is architectural.

## 6. Requirements

Requirement IDs are stable and referenced by every task in `task.md`. Priority: **P0** = 1.0 blocker, **P1** = 1.0 target, **P2** = post-1.0.

### 6.1 Functional — panels and navigation (FR-NAV)

| ID | Requirement | Pri |
|---|---|---|
| FR-NAV-01 | Two independent file panels, side-by-side or stacked, with a draggable and keyboard-adjustable splitter; ratio persisted per session. | P0 |
| FR-NAV-02 | Exactly one panel is *active* (source); the other is *target*. Active panel indicated by cursor rendering and header treatment. Tab switches. | P0 |
| FR-NAV-03 | Each panel hosts N tabs; tabs persist across restart; tabs can be locked (navigation opens a new tab instead) and locked-with-directory-changes-allowed, matching TC semantics. | P0 |
| FR-NAV-04 | Column modes: Full (name, ext, size, date, attrs), Brief (multi-column names), Thumbnails, Tree. Per-tab, persisted. | P0/P1 |
| FR-NAV-05 | Columns are configurable sets (add/remove/reorder/width), including plugin-provided columns; named layouts switchable by keyboard. | P1 |
| FR-NAV-06 | Sorting by any column, ascending/descending, directories-first policy configurable, natural (version) sort for numeric runs, locale-aware collation. | P0 |
| FR-NAV-07 | Quick search within a panel: typing letters jumps to matching entry; a modifier-prefixed mode filters the panel instead of jumping. | P0 |
| FR-NAV-08 | History (back/forward) per tab, and a directory hotlist (bookmarks) with keyboard-invoked overlay. | P0 |
| FR-NAV-09 | Breadcrumb path bar that is also an editable path input; typing a path with completion navigates. | P1 |
| FR-NAV-10 | Branch view (flat recursive listing of the current subtree), with and without hidden files. | P1 |
| FR-NAV-11 | Drive/mount bar listing local block devices, removable media, and active network mounts; mount/unmount/eject actions. | P1 |
| FR-NAV-12 | Directory tree panel synchronised with the active panel, optional. | P1 |

### 6.2 Functional — selection (FR-SEL)

| ID | Requirement | Pri |
|---|---|---|
| FR-SEL-01 | Cursor and selection are independent concepts, as in TC: the cursor is a position, the selection is a set. | P0 |
| FR-SEL-02 | Insert toggles selection and advances; Space toggles selection and computes directory size for the entry under the cursor. | P0 |
| FR-SEL-03 | Select/unselect by wildcard mask, with mask history; invert selection; select all; select same extension; select all with same name. | P0 |
| FR-SEL-04 | Selection persists across sort changes and refreshes where entries still exist; restore-selection-after-operation is configurable. | P0 |
| FR-SEL-05 | Selection statistics in the panel footer: n of m files selected, x of y bytes, live-updating. | P0 |
| FR-SEL-06 | Mouse selection modes: left-click-select (Windows style) and right-click-select (Norton style), configurable. | P1 |

### 6.3 Functional — file operations (FR-OPS)

| ID | Requirement | Pri |
|---|---|---|
| FR-OPS-01 | Copy, move, rename, create directory, delete (to trash and permanently), create symlink and hardlink, with the source→target panel convention. | P0 |
| FR-OPS-02 | All operations run in a background queue with per-job progress, pause/resume, cancel, and reordering; the UI never blocks. | P0 |
| FR-OPS-03 | Two-level progress (current file + total), throughput, ETA, and files-remaining count, refreshed at a fixed cadence independent of I/O chunking. | P0 |
| FR-OPS-04 | Conflict resolution: skip, overwrite, overwrite-if-older, overwrite-if-different-size, rename-target, auto-rename, and *apply to all*; also per-conflict interactive prompt with side-by-side metadata. | P0 |
| FR-OPS-05 | Metadata preservation: mode, mtime/atime, extended attributes, POSIX ACLs, SELinux labels, sparse-file holes, and the hardlink graph within a copy set. Ownership preserved when privileged. | P0 |
| FR-OPS-06 | Same-filesystem moves use `renameat2`; same-filesystem copies attempt reflink (`FICLONE`) then `copy_file_range` then a buffered fallback. | P0 |
| FR-OPS-07 | Crash-safe: an interrupted operation leaves either the old file intact or a clearly-marked partial file, never a silently truncated destination. Journal permits resume. | P0 |
| FR-OPS-08 | Optional post-copy verification (checksum compare) as a per-job flag and a global default. | P1 |
| FR-OPS-09 | Multi-rename tool: pattern-based batch rename with counters, metadata placeholders, regex search/replace, case conversion, live preview, and undo of the last rename batch. | P1 |
| FR-OPS-10 | Directory comparison and synchronisation: compare two trees by name/size/date/content, show differences, and execute a selected sync plan through the operation queue. | P1 |
| FR-OPS-11 | Split and combine files (TC's split/merge with `.crc` verification), and CRC/checksum creation and verification (SFV, MD5, SHA-1, SHA-256, BLAKE3). | P2 |
| FR-OPS-12 | Change attributes/permissions dialog including recursive apply, with symbolic and octal entry, and timestamp editing. | P1 |
| FR-OPS-13 | Elevated operations: when an operation fails with EACCES/EPERM, offer to retry via a polkit-authorised helper rather than requiring the whole app to run as root. | P1 |
| FR-OPS-14 | Undo for the last N reversible operations (move, rename, trash); non-reversible operations are excluded and clearly marked. | P2 |

### 6.4 Functional — virtual filesystems (FR-VFS)

| ID | Requirement | Pri |
|---|---|---|
| FR-VFS-01 | A uniform VFS abstraction; panels, operations, search, and the viewer address everything through it. Local disk is one backend among several. | P0 |
| FR-VFS-02 | Archives browsable as directories: zip, tar (+gz/bz2/xz/zstd), 7z, rar (read), iso, deb, rpm, cab, ar. Enter descends into the archive. | P1 |
| FR-VFS-03 | Archive creation and modification for formats that support it; pack/unpack commands with a format+options dialog. | P1 |
| FR-VFS-04 | Remote backends: SFTP/SSH, FTP/FTPS, WebDAV, S3-compatible object storage, SMB. Connection manager with saved profiles; secrets in the system keyring. | P1 |
| FR-VFS-05 | Nested VFS: an archive inside an archive inside SFTP must work, with a bounded nesting depth and a clear path representation. | P2 |
| FR-VFS-06 | Backends declare capabilities (random read, random write, rename, hardlink, xattr, atomic replace, seek cost); the operation engine adapts strategy rather than assuming POSIX. | P0 |
| FR-VFS-07 | MTP/PTP devices and GVFS mounts are reachable, at minimum by surfacing existing FUSE/GVFS mount points. | P2 |

### 6.5 Functional — viewing, search, tools (FR-TOOL)

| ID | Requirement | Pri |
|---|---|---|
| FR-TOOL-01 | Internal viewer (F3): text with encoding detection and manual override, hex, binary, image, and "auto" mode; search within the viewer; handles multi-GB files without loading them. | P0 |
| FR-TOOL-02 | Quick view panel: the inactive panel shows a live preview of the cursor entry. | P1 |
| FR-TOOL-03 | Internal light editor (F4) with save, encoding and line-ending control; configurable to delegate to an external editor. | P1 |
| FR-TOOL-04 | Search (Alt+F7): name masks, regex, size/date/attribute filters, content search (literal and regex, encoding-aware), search inside archives, and *feed results to panel*. | P0 |
| FR-TOOL-05 | Saved search definitions, reusable as filters. | P2 |
| FR-TOOL-06 | Command line at the bottom of the window that executes in the user's shell with the active panel as cwd, with history, completion, and keys to insert the current filename/path. | P0 |
| FR-TOOL-07 | Embedded terminal panel (toggleable) sharing the active panel's cwd. | P2 |
| FR-TOOL-08 | File associations: open with default application, "Open With" menu built from the desktop-entry database, and internal association overrides. | P0 |
| FR-TOOL-09 | Thumbnails for images, video, PDF, and anything covered by installed `*.thumbnailer` entries; compliant on-disk thumbnail cache. | P1 |
| FR-TOOL-10 | Properties dialog: metadata, permissions, ownership, xattrs, checksums on demand, and per-type extended metadata from content plugins. | P1 |
| FR-TOOL-11 | Command palette listing every registered command with its binding, fuzzy-searchable. | P0 |
| FR-TOOL-12 | Button bar (TC's toolbar) that is user-editable and can invoke commands, external programs, or menus. | P2 |

### 6.6 Functional — configuration and integration (FR-CFG)

| ID | Requirement | Pri |
|---|---|---|
| FR-CFG-01 | All configuration is plain-text TOML under XDG paths, hot-reloaded on change, with a settings UI that edits the same files. | P0 |
| FR-CFG-02 | Keymap is a separate file; the default keymap is TC-faithful; alternative base keymaps (mc, Dolphin-ish) are selectable; user overrides layer on top. | P0 |
| FR-CFG-03 | Import tool for `wincmd.ini` (colours, hotlist, button bar, associations) from an existing TC install. | P2 |
| FR-CFG-04 | Theming: named themes with a documented token set, dark/light following the desktop preference by default; icon theme resolution from the XDG icon theme. | P1 |
| FR-CFG-05 | Clipboard interop: cut/copy files to and from other file managers via `text/uri-list` plus the GNOME and KDE cut-marker conventions. | P0 |
| FR-CFG-06 | Drag and drop within Duet, and to/from other applications, on both Wayland and X11. | P1 |
| FR-CFG-07 | Trash implementing the freedesktop trash specification, including top-level `.Trash-$uid` on other mounts, plus a trash browser. | P0 |
| FR-CFG-08 | `org.freedesktop.FileManager1` D-Bus interface so other applications' "Show in file manager" reaches Duet; single-instance activation. | P1 |
| FR-CFG-09 | CLI: `duet [--left DIR] [--right DIR] [--new-tab] PATH...`, plus `--goto FILE` to open and highlight. | P1 |
| FR-CFG-10 | Localisation infrastructure (Fluent), with English complete and the string catalogue extractable. | P1 |

### 6.7 Functional — plugins (FR-PLUG)

| ID | Requirement | Pri |
|---|---|---|
| FR-PLUG-01 | Plugins are WebAssembly components, loaded out of the host's address space, with no ambient filesystem or network authority. | P1 |
| FR-PLUG-02 | Five plugin classes: **filesystem** (mount a VFS backend), **packer** (archive format), **content** (custom columns and metadata fields), **viewer** (custom preview), **command** (register commands and menu items). The first four mirror TC's WFX/WCX/WDX/WLX. | P1 |
| FR-PLUG-03 | Capability grants are declared in the manifest, shown to the user at install time, and enforced at the host boundary. | P1 |
| FR-PLUG-04 | A plugin registry (a git-hosted index) with install/update/remove from inside the app, plus local `--dev` loading from a directory. | P2 |
| FR-PLUG-05 | An SDK crate and a `cargo generate` template such that a "custom column" plugin is under 50 lines. | P1 |
| FR-PLUG-06 | A misbehaving plugin (panic, infinite loop, memory blowup) degrades its own feature only; fuel/epoch limits and memory caps are enforced. | P1 |

### 6.8 Non-functional (NFR)

| ID | Requirement | Target | Measurement |
|---|---|---|---|
| NFR-01 | Cold start to interactive | ≤ 150 ms | `hyperfine` on warm page cache, instrumented "first frame with real listing" marker |
| NFR-02 | Keystroke-to-pixel latency | p50 ≤ 6 ms, p99 ≤ 12 ms | In-process frame instrumentation; cross-checked with a high-speed camera once |
| NFR-03 | Directory listing, 100k entries | first paint ≤ 100 ms, fully sorted ≤ 400 ms | Bench harness on tmpfs and ext4 |
| NFR-04 | Directory listing, 1M entries | usable (scrollable, sortable) ≤ 3 s, no UI stall > 16 ms | Bench harness |
| NFR-05 | Scroll performance | sustained monitor refresh rate on 1M rows | Frame-time histogram, no frame > 8.3 ms at 120 Hz |
| NFR-06 | Memory | ≤ 150 MB RSS with two panes × 100k entries and thumbnails off | `/proc/self/status` sampling in bench |
| NFR-07 | Copy throughput | ≥ 95% of `cp` for large files; ≥ 80% of `cp -a` for 100k small files | Bench against coreutils on the same disk |
| NFR-08 | Data integrity | zero loss/corruption across the crash-injection suite | §14.4 suite must pass 100% |
| NFR-09 | Binary size | ≤ 40 MB stripped, ≤ 25 MB for the core without bundled archive codecs | CI check |
| NFR-10 | Startup dependencies | runs on a minimal Wayland or X11 session with no GTK/Qt/KDE runtime | Container test matrix |
| NFR-11 | Accessibility | keyboard-complete (no mouse-only actions); screen-reader support is a documented gap with a tracked plan | Manual audit; §17 OQ-4 |
| NFR-12 | Crash rate | < 0.1% of sessions in beta telemetry (opt-in only) | Optional crash reporter |

## 7. Technology selection

### 7.1 The question actually being asked

"Is GPUI a good base for this?" splits into three:

1. Can GPUI render a 1M-row virtualised table at 120 Hz with sub-frame input latency on Linux? — **Almost certainly yes.** That is precisely the workload Zed's editor and project panel exercise, and the framework is built around a retained entity model with an immediate-mode element tree and GPU-side quad/text batching.
2. Does GPUI give a file manager the *platform integration* it needs — clipboard with custom MIME types, cross-application drag and drop, native-ish dialogs, accessibility? — **Unknown, and this is the real risk.** These are editor-irrelevant capabilities, so Zed has had no pressure to build them. See §7.4.
3. Is the toolkit even the hard part? — **No.** Perhaps 20–25% of the effort in this document is UI. The VFS layer, the operation engine, and the plugin runtime dominate, and they are toolkit-independent.

Answer 3 dictates the architecture: build the core so it does not know GPUI exists (§7.5, ADR-002).

### 7.2 GPUI: what it is today

GPUI is a hybrid immediate/retained-mode, GPU-accelerated Rust UI framework from the Zed team. Relevant current facts:

- It is published to crates.io as `gpui`, with platform backends split into `gpui_platform` (feature-gated `wayland`, `x11`, `font-kit`). It remains **pre-1.0 with frequent breaking changes**, and the maintainers say so explicitly.
- State lives in `Entity<T>` handles owned by the framework; views are entities implementing `Render`; elements are styled with a Tailwind-like builder API; layout is Taffy-based flexbox.
- It ships an action/keybinding system (named actions dispatched through a focus tree with context predicates) — a near-exact match for FR-CFG-02 and G-4.
- It provides a foreground/background executor pair integrated with the platform event loop, and `TestAppContext` for headless UI testing (which makes §14.3 possible).
- **`gpui-component`** (community, by Longbridge) supplies the widget layer: virtualised `Table` and `List`, dock/panel layout with resizable and tiled arrangements, input, select, combobox, context menus, dialogs, notifications, markdown rendering, a theme system, and a tree-sitter-backed code editor component. This is a very large head start — the virtualised table alone is the single most important widget in this product.

### 7.3 Alternatives considered

| Option | Verdict |
|---|---|
| **GPUI + gpui-component** | **Chosen.** Best latency ceiling; virtualised table and dock layout exist; keymap/action system matches our command model; Rust ecosystem for the core is excellent. |
| Iced | Elm-ish, mature-ish, pure Rust, good Wayland story. Weaker virtualised-table story; retained/immediate hybrid ergonomics for a huge table are less proven. Viable fallback. |
| Slint | Excellent tooling and a real designer story; embedded focus. DSL boundary adds friction for a keyboard-command-heavy app. |
| egui | Simplest, truly immediate mode. Text rendering and native integration are the weak spots for a shipping desktop app. |
| Qt (via cxx-qt) / KDE Frameworks | Best integration and the widest protocol reach (KIO). But it is Krusader's position, and it re-imports the weight the product exists to avoid. |
| GTK4 (gtk-rs) | Great integration, `GtkColumnView` is virtualised. Loses the differentiator: it will feel like other GTK apps, and GTK's input-to-paint path is not the target. |
| Tauri/Electron | Rejected on NFR-01/02/06 grounds. |

**ADR-001: Use GPUI + gpui-component for the shell.** Accepted, conditional on Phase 0 spikes S-1…S-6 (see `task.md` §Phase 0). If S-2 (clipboard) or S-3 (drag and drop) fail with no tractable workaround, the decision is revisited at G0 — with Iced the most likely substitute, and the cost bounded by ADR-002.

**ADR-002: The core is UI-framework-agnostic.** No crate below `duet-ui` may depend on `gpui`. Cross-layer communication is by plain Rust types, channels, and a small `Executor` trait the shell implements. Consequence: swapping the shell costs the UI crates only (~25% of the codebase), and the core is testable headlessly and reusable from a future TUI or CLI.

**ADR-003: Pin GPUI to a specific version and update deliberately.** Given documented breaking changes, pin an exact version, keep a `gpui-compat` shim module for anything churn-prone, and schedule bumps as explicit tasks with a compile-and-smoke gate rather than picking them up implicitly.

### 7.4 GPUI risk register (specific, not generic)

| ID | Risk | Impact | Mitigation |
|---|---|---|---|
| R-G1 | Pre-1.0 breaking changes churn the UI layer | Medium, recurring | ADR-003 pinning; `gpui-compat` shim; UI layer kept thin |
| R-G2 | Clipboard cannot carry `text/uri-list` and the GNOME/KDE cut markers | **High — FR-CFG-05 is P0** | Spike S-2. Fallback: own the clipboard at the protocol layer (`wl_data_device` via smithay-client-toolkit; `x11rb` selections) alongside GPUI's window, or upstream a MIME-generic clipboard API |
| R-G3 | Cross-application drag and drop absent or one-directional | High — FR-CFG-06 | Spike S-3. Same fallback shape as R-G2. Degrade to P1-deferred if only intra-app DnD is achievable for 1.0 |
| R-G4 | No accessibility tree (AT-SPI) | Medium — NFR-11 | Ship keyboard-complete; document the gap; investigate AccessKit's AT-SPI adapter and, if viable, upstream to GPUI post-1.0 |
| R-G5 | Font/icon/cursor theme integration weaker than native toolkits | Low–Medium | Read fontconfig for the UI font, XDG icon theme for icons, XCursor for cursors; verify on the compositor matrix |
| R-G6 | HiDPI, fractional scaling, multi-monitor mixed-DPI defects | Medium | Test matrix in §14.5; these are known-hard on Wayland and must be tested continuously, not at the end |
| R-G7 | `gpui-component` is a single-maintainer-ish community project tracking a moving upstream | Medium | Vendor a pinned copy; keep our usage inside a `duet-widgets` façade so a fork or replacement is local |
| R-G8 | IME/complex text input in the path bar and rename fields | Low–Medium | Test CJK IME early (S-6); it is the kind of thing discovered too late |
| R-G9 | Native file/folder chooser needed for some flows | Low | Use `xdg-desktop-portal` via zbus; also acceptable to use Duet's own chooser |

### 7.5 Language and dependency policy

Rust, edition 2024, latest stable. Dependency additions require a note in the ADR log covering: license, maintenance signal, transitive weight, and whether it can be feature-gated. Archive and network codecs are feature-gated so the base binary stays inside NFR-09.

Notable intended dependencies: `tokio` (I/O and the operation runtime), `rustix` (raw syscalls without libc unsafety sprawl), `notify` (inotify), `walkdir`/custom traversal, `rayon` (bulk stat/hash), `zbus` (D-Bus: udisks2, portals, FileManager1), `keyring`/`secret-service` (credentials), `wasmtime` (plugins), `russh` or `libssh2` (SFTP), `opendal` (S3/WebDAV/FTP breadth), `zip`/`tar`/`sevenz-rust2`/`unrar` (archives), `image` (decode), `blake3`/`sha2` (hashing), `fluent` (i18n), `serde`/`toml`, `tracing`.

## 8. Architecture

### 8.1 Crate layout

```
duet/
├── crates/
│   ├── duet-types/        # VPath, EntryId, Metadata, Capabilities, error taxonomy. No deps beyond std/serde.
│   ├── duet-vfs/          # FileSystem trait, registry, mount table, path resolution
│   │   ├── local/         #   POSIX backend (the fast path)
│   │   ├── archive/       #   archive-as-directory backends
│   │   └── remote/        #   sftp, ftp, webdav, s3, smb
│   ├── duet-ops/          # operation engine: planner, executor, journal, queue, conflict policy
│   ├── duet-index/        # directory model: listing, watching, sorting, filtering, caching
│   ├── duet-search/       # name + content search, mask/regex compiler, result streaming
│   ├── duet-meta/         # mime detection, icon lookup, thumbnails, desktop entries, associations
│   ├── duet-commands/     # command registry, keymap parsing/resolution, command palette index
│   ├── duet-config/       # settings, themes, hotlist, persistence, hot reload
│   ├── duet-plugin/       # wasmtime host, WIT bindings, capability enforcement, registry client
│   ├── duet-platform/     # clipboard, DnD, trash, mounts, D-Bus, polkit helper client, portals
│   ├── duet-widgets/      # GPUI façade over gpui-component (isolation layer for R-G7)
│   ├── duet-ui/           # panels, tabs, dialogs, viewer, editor, palette — the only GPUI-aware layer
│   └── duet/              # binary: wiring, CLI, single-instance, lifecycle
├── helpers/
│   └── duet-privileged/   # small setuid-free polkit-activated helper for FR-OPS-13
├── plugins-sdk/           # WIT definitions + Rust SDK + template
├── benches/               # criterion + custom harnesses for NFR-01..07
└── tests/                 # integration, conformance, fault-injection suites
```

Dependency rule (CI-enforced by `cargo-deny`-style graph lint): `duet-ui` and `duet-widgets` may depend on `gpui`; **nothing else may**.

### 8.2 Process and thread model

- **Main/UI thread.** Runs GPUI's event loop and the foreground executor. Does no I/O, no `stat`, no hashing, no syscalls that can block on a network filesystem. Ever. This one rule is most of NFR-02.
- **I/O runtime.** A multi-threaded Tokio runtime for VFS work: listings, remote protocols, archive reads. Sized to `min(8, cpus)`.
- **Operation workers.** Separate from the I/O runtime so that a saturated copy queue cannot starve interactive listing. Default concurrency: one worker per distinct *device* for rotational media (detected via `/sys/block/*/queue/rotational`), up to four for NVMe, configurable.
- **CPU pool.** Rayon for hashing, thumbnail decode, bulk sort, and content search.
- **Plugin host.** In-process `wasmtime` engine but with per-instance fuel/epoch interruption and memory limits; each plugin instance is pinned to the I/O runtime, never the UI thread. A future out-of-process mode is designed for but not built (§9.9).

Message flow is one-directional: UI → command → core service (async task) → event stream → UI applies a diff. No core component holds a UI handle; the UI subscribes.

### 8.3 High-level component diagram (textual)

```
        ┌──────────────────────── duet-ui (GPUI) ────────────────────────┐
        │  Workspace                                                     │
        │  ├── PanelView(left)   ── TabStrip ── FileTable(virtualised)    │
        │  ├── PanelView(right)  ── TabStrip ── FileTable/QuickView       │
        │  ├── CommandLine · FunctionBar · StatusBar · OperationTray      │
        │  └── Overlays: palette, dialogs, viewer, editor, search         │
        └───────▲──────────────────────────────────┬─────────────────────┘
                │ events (diffs, progress, errors)  │ commands
        ┌───────┴──────────────────────────────────▼─────────────────────┐
        │ duet-commands  (registry · keymap · dispatch · palette index)   │
        └───────┬──────────────────────────────────┬─────────────────────┘
                │                                  │
     ┌──────────▼─────────┐  ┌────────────────┐  ┌─▼───────────────────┐
     │ duet-index         │  │ duet-ops       │  │ duet-search         │
     │ listing/watch/sort │  │ queue/journal  │  │ name+content        │
     └──────────┬─────────┘  └────────┬───────┘  └─┬───────────────────┘
                └──────────┬──────────┴────────────┘
                           ▼
                 ┌───────────────────┐        ┌──────────────────┐
                 │ duet-vfs          │◄───────┤ duet-plugin      │
                 │ local/archive/net │        │ wasm host + caps │
                 └─────────┬─────────┘        └──────────────────┘
                           ▼
        ┌──────────────────────────────────────────────────────────┐
        │ duet-platform: trash · clipboard · DnD · udisks2 · portal │
        └──────────────────────────────────────────────────────────┘
```

## 9. Component design

### 9.1 `duet-vfs` — the virtual filesystem layer

The central abstraction. Everything above it addresses files by `VPath`, never by `std::path::Path`.

```rust
/// Location within a mounted filesystem. Rendered as e.g.
///   file:///home/u/x.zip           (a local file)
///   zip:file:///home/u/x.zip!/a/b  (inside that archive)
///   sftp://host/srv/logs           (remote)
pub struct VPath { mount: MountId, inner: UnixPathBuf }

bitflags! {
    pub struct Caps: u32 {
        const RANDOM_READ      = 1 << 0;  // seekable reads
        const RANDOM_WRITE     = 1 << 1;
        const RENAME           = 1 << 2;
        const ATOMIC_REPLACE   = 1 << 3;  // rename-over semantics
        const HARDLINK         = 1 << 4;
        const SYMLINK          = 1 << 5;
        const XATTR            = 1 << 6;
        const PERMISSIONS      = 1 << 7;
        const TIMESTAMPS       = 1 << 8;
        const SPARSE           = 1 << 9;
        const REFLINK          = 1 << 10;
        const WATCH            = 1 << 11; // can push change events
        const CHEAP_STAT       = 1 << 12; // stat is not a round-trip
        const APPEND_RESUME    = 1 << 13; // interrupted writes can resume
    }
}

#[async_trait]
pub trait FileSystem: Send + Sync {
    fn scheme(&self) -> &'static str;
    fn caps(&self) -> Caps;
    /// Streaming listing: the panel renders the first chunk before the last arrives.
    fn read_dir(&self, p: &VPath, opts: ListOpts) -> BoxStream<'_, Result<Vec<DirEntry>>>;
    async fn stat(&self, p: &VPath, follow: bool) -> Result<Metadata>;
    async fn open_read(&self, p: &VPath) -> Result<Box<dyn AsyncReadSeek>>;
    async fn open_write(&self, p: &VPath, o: WriteOpts) -> Result<Box<dyn AsyncWriteCommit>>;
    async fn create_dir(&self, p: &VPath, mode: Option<Mode>) -> Result<()>;
    async fn remove(&self, p: &VPath, kind: RemoveKind) -> Result<()>;
    async fn rename(&self, from: &VPath, to: &VPath, flags: RenameFlags) -> Result<()>;
    async fn set_meta(&self, p: &VPath, m: &MetaPatch) -> Result<()>;
    fn watch(&self, p: &VPath) -> Result<BoxStream<'_, ChangeEvent>>;
    /// Backend-accelerated same-filesystem copy; returns Unsupported to fall back.
    async fn server_side_copy(&self, from: &VPath, to: &VPath) -> Result<CopyOutcome>;
}
```

Design notes:

- **`AsyncWriteCommit`** separates writing from publishing. Implementations write to a temporary sibling and `commit()` renames over the destination (`ATOMIC_REPLACE`), or, where that is impossible (FTP), write in place and report reduced durability so the engine can warn.
- **Capability-driven strategy, not capability-driven refusal.** If `REFLINK` is absent, the engine degrades to `copy_file_range`, then to buffered copy. If `CHEAP_STAT` is absent (SFTP, S3), the listing engine batches metadata and the sorter tolerates late-arriving fields rather than blocking.
- **`ListOpts`** carries which metadata fields the caller actually needs. A brief-mode panel asks for names only; a full-mode panel asks for size/mtime/mode. On remote backends this is the difference between one round trip and ten thousand.
- **Mount table.** `MountId → Arc<dyn FileSystem>`, plus the parent link that makes nesting work (`zip:` mounted *on* a `file:` path). Reference-counted; an archive mount is torn down when the last tab, operation, and viewer referencing it are gone.
- **Local backend fast path.** Uses `getdents64` through `read_dir`, exploits `d_type` to skip `stat` for entries where the panel doesn't need more, and issues `statx(AT_STATX_DONT_SYNC)` in parallel batches on the CPU pool for the rest. All path traversal uses `*at` syscalls relative to an open directory FD to avoid TOCTOU and repeated path walks.

### 9.2 `duet-index` — listing, watching, and the panel model

Each panel tab owns a `DirectoryModel`:

```
DirectoryModel {
    entries:    EntryStore,      // struct-of-arrays, see below
    order:      Vec<u32>,        // sorted+filtered indices — this is what the table renders
    selection:  RoaringBitmap,   // by stable EntryId
    cursor:     EntryId,
    generation: u64,             // bumped on every mutation; drives cheap diffing
}
```

- **Struct-of-arrays.** Names in one interned arena (`Box<str>` slabs, no per-entry `String` allocation), sizes in a `Vec<u64>`, times in a `Vec<i64>`, flags in a `Vec<u32>`. Sorting permutes `order`, never the data. This is what makes 1M entries feasible inside NFR-06.
- **Streaming population.** `read_dir` chunks arrive on the I/O runtime and are applied to the model on a fixed cadence (one flush per frame, max), so a huge directory paints progressively instead of stalling.
- **Sorting.** Locale-aware collation with a natural-numeric mode, precomputed sort keys for the active column so comparisons are integer or byte-slice compares. Stable across refreshes so the cursor doesn't jump.
- **Watching.** `notify`/inotify with a 50 ms debounce and coalescing; `IN_Q_OVERFLOW` triggers a full rescan; network and FUSE mounts fall back to interval polling with a configurable period. Backends that lack `WATCH` get polling from this layer, not from the backend.
- **Directory sizes.** Computing a directory's recursive size is an explicit, cancellable background job (Space key, or "calculate all"), with results cached by `(dev, ino, mtime)` and invalidated by watch events.

### 9.3 `duet-ops` — the operation engine

The most safety-critical component. Structure: **plan → execute → journal**.

**Planning.** A job is a `Plan`: a materialised, ordered list of `Step`s produced by walking the source set. Planning is itself async and cancellable and produces the totals (files, bytes) that make honest progress possible. Steps are concrete: `CreateDir`, `CopyFile`, `Reflink`, `Rename`, `Link`, `SetMeta`, `Remove`, `Verify`.

**Copy strategy ladder** (local → local):

1. `ioctl(FICLONE)` — instant, if same filesystem supports reflink (btrfs, xfs with reflink=1, bcachefs).
2. `copy_file_range(2)` — kernel-side copy, avoids userspace bounce.
3. Sparse-aware buffered copy: `SEEK_HOLE`/`SEEK_DATA` to skip holes, 1–4 MiB buffers, `posix_fadvise(POSIX_FADV_DONTNEED)` on the source behind the read cursor so a 200 GB copy doesn't evict the user's page cache.
4. `io_uring` batching for many small files — feature-gated, benchmarked in Phase 9, adopted only if it wins by >15% (T-9.3.5).

**Move.** Same `st_dev` → `renameat2`. Cross-device → copy, verify (if enabled), `fsync`, then unlink source. Never unlink before the destination is durable.

**Metadata.** After content: mode, then xattrs (`listxattr`/`setxattr`, skipping `security.*` unless privileged and requested), POSIX ACLs, SELinux label, then timestamps *last* (`utimensat`, because writing xattrs perturbs ctime), then ownership if privileged.

**Hardlink graph.** A `HashMap<(dev, ino), VPath>` for the duration of a job; the second occurrence of an inode becomes a `Link` step rather than a second copy. This preserves the structure of e.g. a rsnapshot tree and is a thing users notice when it's missing.

**Conflicts.** Policy is a resolved enum per step, decided by: job-level default → per-conflict user answer → "apply to all" answers. The prompt shows both sides (size, mtime, and a hash on request) and offers the TC set: skip / overwrite / overwrite-if-older / overwrite-all / rename / auto-rename / abort.

**Journal (FR-OPS-07).** Each job appends to `~/.local/state/duet/jobs/<id>.journal`, an fsync'd append-only record of intended and completed steps. Guarantees:

- Destination files are written to `.duet-partial-<rand>` and renamed into place only when complete and (optionally) verified. A SIGKILL therefore leaves the old destination intact and a visible partial file.
- On next launch, incomplete journals surface as "3 interrupted operations — review". The user can resume (re-plan the remainder, using `APPEND_RESUME` where the backend allows), discard partials, or inspect.
- Deletes are journaled before execution so the undo stack (FR-OPS-14) has something to work from for trash operations.

**Queue.** Jobs are queued, not fire-and-forget. UI surface: a tray in the status bar showing aggregate progress, expanding to a full manager with per-job pause/resume/cancel/reorder/priority and an errors-and-skips list that can be re-run.

**Progress.** Updated on a 100 ms timer sampling atomic counters — decoupled from I/O chunk size so throughput never distorts the refresh rate. ETA uses an exponentially-weighted moving average with separate small-file and large-file regimes, because a naive average lies badly on mixed sets.

**Error taxonomy.** Every failure is classified as `Retryable` (EINTR, EAGAIN, transient network), `Permission` (→ offer FR-OPS-13 elevation), `Space` (ENOSPC/EDQUOT → pause the whole queue, not just the job), `NotFound`, `Conflict`, or `Fatal`. Retryable errors get bounded exponential backoff. The job never dies silently: it ends `Completed`, `CompletedWithSkips`, `Cancelled`, or `Failed`, always with a report.

### 9.4 `duet-commands` — commands and keymap

```toml
# keymap.toml — the TC-faithful default, layered by user overrides
[[binding]]
context = "panel"
key     = "f5"
command = "ops.copy"

[[binding]]
context = "panel && selection.nonempty"
key     = "shift-f6"
command = "ops.rename_in_place"
```

- A **command** is `{ id, title, category, args_schema, precondition, handler }`. Everything the app can do is registered here, including plugin-provided commands. This satisfies G-4 and makes FR-TOOL-11 (palette) and FR-CFG-02 (rebinding) fall out for free.
- **Contexts** are boolean predicates over UI state (`panel`, `viewer`, `editor`, `dialog`, `cmdline`, plus state terms like `selection.nonempty`, `entry.is_dir`, `vfs.scheme == 'file'`). GPUI's own action-dispatch context system is used as the substrate.
- **Chord support** for multi-key sequences, with a pending-chord indicator, since some TC bindings and most user customisations want it.
- **Base keymaps** ship as separate files: `tc.toml` (default), `mc.toml`, `modern.toml`. User file layers on top and can `unbind`.
- **Conflict detection** at load time with a diagnostic surface — silently shadowed bindings are a classic source of "the app is broken" reports.

Default keymap extract (full table in Appendix A; every entry validated against TC 11 in T-1.4.2):

| Key | Command | Key | Command |
|---|---|---|---|
| Tab | focus.other_panel | F2 | panel.reread |
| F3 | view.open | F4 | edit.open |
| F5 | ops.copy | F6 | ops.move_or_rename |
| F7 | ops.mkdir | F8 / Del | ops.delete |
| Ins | sel.toggle_and_advance | Space | sel.toggle_and_size |
| Num + / − / \* | sel.by_mask / unsel.by_mask / sel.invert | Ctrl+U | panel.swap |
| Ctrl+←/→ | panel.push_to_other | Ctrl+PgUp | nav.parent |
| Ctrl+\\ | nav.root | Ctrl+D | hotlist.open |
| Ctrl+T / Ctrl+W | tab.new / tab.close | Ctrl+B | panel.branch_view |
| Ctrl+M | tool.multi_rename | Ctrl+Q | panel.quick_view |
| Alt+F7 | tool.search | Alt+F5 / Alt+F9 | archive.pack / archive.unpack |
| Alt+F1 / Alt+F2 | drive.change_left / right | Alt+Enter | file.properties |
| Ctrl+Enter | cmdline.insert_name | Ctrl+Shift+Enter | cmdline.insert_path |

### 9.5 `duet-ui` — the shell

- **Workspace** owns two `PanelView`s, the splitter, command line, function bar, status bar, and the operation tray; overlays (palette, dialogs, viewer) are rendered above via a layered root.
- **FileTable** is the performance-critical view. It renders only visible rows (from `gpui-component`'s virtualised `Table`, wrapped by `duet-widgets`), reading directly from the `EntryStore` columns by index. No per-frame allocation, no per-row `String` formatting: size and date strings are formatted into a per-frame arena and cached by value.
- **Quick view** swaps a panel's table for a preview surface driven by the same viewer stack as F3 (§9.6), so there is one preview implementation, not two.
- **Dialogs** are modal overlays with a shared focus-trap and a keyboard-complete contract (NFR-11): every dialog is fully operable with Tab/arrows/Enter/Escape, verified by a test.
- **Rendering discipline.** The UI subscribes to model generations and re-renders on change; nothing polls. Long strings are elided at layout time using measured text, not character counts.

### 9.6 Viewer and editor

- **Viewer** is a mode dispatcher over a `Source` (any `AsyncReadSeek` from the VFS): `Text`, `Hex`, `Image`, `Media` (metadata only in 1.0), `Plugin`.
- **Text mode** never loads the whole file. A line-index is built incrementally in the background (offset table, chunked); scrolling seeks. Encoding detection via BOM → `chardetng` heuristics → user override; the detected encoding is shown and switchable. Files with no line breaks (minified JS, single-line JSON) fall back to wrapped fixed-width chunking so they don't blow up the indexer.
- **Search in viewer** streams over chunks with an overlap window so matches spanning chunk boundaries are found.
- **Hex mode** with configurable width, ASCII gutter, and offset base.
- **Editor** wraps `gpui-component`'s code editor (rope-backed, tree-sitter highlighting). Saves through `AsyncWriteCommit` so the atomic-replace guarantee applies to edits too. Explicitly *not* a Zed competitor; a "delegate to external editor" setting is first-class.

### 9.7 `duet-search`

Two-stage: a **traversal** stage (parallel walk, respecting mount boundaries and a `--one-filesystem` flag, with symlink-loop detection via a visited-inode set) feeding a **matcher** stage (name masks compiled to a matcher; regex via `regex`; content search via `grep-searcher`/`memchr` with encoding handling and a binary-file heuristic). Results stream to the UI as they arrive and can be *fed to a panel* (FR-TOOL-04) as a synthetic listing — which is the feature that makes search actually useful in an OFM, because the result set then supports the entire selection and operation model.

### 9.8 `duet-meta` — MIME, icons, thumbnails, associations

- MIME by extension (shared-mime-info database) with magic-byte sniffing fallback; a small in-memory LRU keyed by extension so a 100k-entry directory does ~30 distinct lookups, not 100k.
- Icons resolved from the XDG icon theme with the standard inheritance chain, rasterised once per (icon, size, scale) and cached in a GPU atlas.
- Thumbnails follow the freedesktop thumbnail spec (`~/.cache/thumbnails/{normal,large,x-large}`, `Thumb::URI`/`Thumb::MTime` validation, `fail/` directory for known-bad). Generation order: internal decoders for common images → installed `/usr/share/thumbnailers/*.thumbnailer` subprocesses (sandboxed with a timeout and a memory cap) for everything else. Using the system thumbnailers is what buys PDF, video, and SVG support without writing decoders.
- Associations: parse `mimeapps.list` and the desktop-entry database; "Open With" lists real applications; launching uses `gio`-equivalent semantics implemented directly (field codes `%f %F %u %U`, `Terminal=true` handling, `DBusActivatable`).

### 9.9 `duet-plugin` — the extension system

**Runtime.** `wasmtime` with the Component Model; interfaces defined in WIT; WASI Preview 2 with *no* preopened directories by default.

**WIT sketch:**

```wit
package duet:plugin@0.1.0;

interface host {
    record entry { name: string, size: u64, mtime: s64, mode: u32, is-dir: bool }
    /// Only paths the host has granted. Enforced host-side, not trust-based.
    open-granted: func(handle: u32) -> result<stream, error>;
    progress: func(done: u64, total: u64) -> bool;      // false => user cancelled
    log: func(level: level, msg: string);
    secret-get: func(key: string) -> result<string, error>;   // requires cap:secrets
}

interface content-plugin {            // ≈ TC .wdx
    fields: func() -> list<field-def>;
    value: func(path: string, field: u32) -> result<field-value, error>;
}

interface packer-plugin {             // ≈ TC .wcx
    probe: func(head: list<u8>, name: string) -> bool;
    list: func(archive: u32) -> result<list<entry>, error>;
    extract: func(archive: u32, member: string, out: u32) -> result<_, error>;
    can-write: func() -> bool;
    add: func(archive: u32, member: string, src: u32) -> result<_, error>;
}

interface fs-plugin { /* ≈ TC .wfx: connect, list, get, put, remove, mkdir, rename */ }
interface viewer-plugin { /* ≈ TC .wlx: probe + render-to-surface or render-to-markdown */ }
interface command-plugin { /* register commands, receive invocation with selection */ }
```

**Capability model.** Manifest declares needs; the host enforces:

```toml
[plugin]
id = "exif-columns"; version = "0.2.1"; kind = "content"
[capabilities]
read-files = ["*.jpg", "*.jpeg", "*.tiff"]   # host opens and hands over a handle; plugin never has a path
network    = false
secrets    = []
```

The plugin never receives a filesystem path it can open. It receives *handles* the host opened after checking the grant. That is the whole security argument, and it is why this is a WASM design rather than a `.so` design.

**Resource limits.** Per-instance: memory cap (default 64 MiB), fuel or epoch-based interruption (default 2 s per call for content plugins, unbounded-with-cancellation for fs plugins), and a panic → instance restart with the feature marked degraded (FR-PLUG-06).

**Distribution.** A git-hosted index repo of manifests pointing at release artifacts (the model Zed uses for extensions). `duet plugin install <id>`, plus in-app browse/install/update. `--dev-plugin <dir>` for local iteration.

**Native bridge (post-1.0, OQ-7).** An out-of-process `duet-native-plugin-host` speaking the same protocol over a pipe could host real C-ABI plugins and, under Wine, actual TC `.wcx`/`.wdx`. Designed-for (the protocol is transport-agnostic), not built.

### 9.10 `duet-platform` — desktop integration

| Concern | Approach | Risk |
|---|---|---|
| Trash | Direct freedesktop trash-spec implementation: `~/.local/share/Trash/{files,info}`, `$topdir/.Trash/$uid` or `$topdir/.Trash-$uid` for other mounts, correct `.trashinfo` with relative paths | Low |
| Clipboard | `text/uri-list` + `x-special/gnome-copied-files` + `application/x-kde-cutselection` | **R-G2 — spike S-2** |
| DnD | Wayland `wl_data_device` / X11 XDND, both directions | **R-G3 — spike S-3** |
| Mounts | `udisks2` over zbus for block devices; parse `/proc/self/mountinfo` for the live picture; surface GVFS mounts under `/run/user/$UID/gvfs` | Low |
| Elevation | polkit action + a tiny D-Bus-activated helper exposing only `copy/move/delete/chmod/chown` on validated paths — never `pkexec duet` | Medium (§13) |
| Open URI / portals | `xdg-desktop-portal` via zbus for `OpenURI`, `Trash` fallback, and screenshot-free file chooser when embedding demands it | Low |
| FileManager1 | Implement `org.freedesktop.FileManager1` (`ShowFolders`, `ShowItems`, `ShowItemProperties`) | Low |
| Single instance | D-Bus name ownership; second launch forwards its CLI to the running instance unless `--new-instance` | Low |

## 10. Data model and file formats

```
~/.config/duet/
    settings.toml          # everything except keys and themes
    keymap.toml            # user layer
    keymaps/tc.toml        # shipped bases (read-only, copied on customise)
    hotlist.toml           # bookmarks
    connections.toml       # remote profiles (no secrets — keyring refs only)
    themes/*.toml
    plugins/<id>/          # installed plugin bundles + per-plugin config
~/.local/state/duet/
    session.json           # panes, tabs, cwds, cursor positions, sort state
    history/               # command line, masks, search, paths
    jobs/*.journal         # operation journals (§9.3)
~/.cache/duet/
    dirsize.db             # directory size cache
    icons/                 # rasterised icon cache
```

`settings.toml` sketch:

```toml
[panels]
sort_directories_first = true
natural_sort           = true
show_hidden            = false
default_view           = "full"          # full | brief | thumbnails | tree

[operations]
verify_after_copy      = false
preserve_xattrs        = true
preserve_acls          = true
concurrency            = "auto"          # auto | 1 | 2 | 4 ...
delete_default         = "trash"         # trash | permanent
confirm_delete         = "always"        # always | non-empty-dirs | never

[appearance]
theme      = "system"
font       = "system-ui"
font_size  = 13
row_height = "compact"

[terminal]
shell = "$SHELL"
```

**Compatibility policy.** Config files carry a `schema_version`. Migrations are forward-only and run on load with a backup written alongside. Unknown keys are preserved on rewrite (round-trip-preserving TOML) so a newer Duet's settings survive an older Duet touching them.

## 11. Performance design

The NFR targets in §6.8 are met by five specific decisions, each with an owning task:

1. **Nothing blocking on the UI thread.** Enforced by a debug-build assertion: a guard on the UI thread that panics if a syscall wrapper from `duet-vfs` is entered (T-3.1.6).
2. **Struct-of-arrays entry storage with an interned name arena** (§9.2) — kills allocation pressure and cache misses on the sort/scan path.
3. **Streaming listings with frame-cadence flushes** — decouples "directory is huge" from "UI is stuck".
4. **Virtualised rendering** — the table renders O(visible), not O(entries), and formats strings only for visible rows.
5. **Metadata laziness** — the panel requests only the columns it displays; expensive fields (directory sizes, checksums, plugin columns, thumbnails) are computed on demand, cached, and rendered as placeholders until ready.

A continuous benchmark suite (`benches/`) runs in CI on a fixed runner with a generated corpus (`10`, `1k`, `100k`, `1M` entries; deep trees; long names; unicode names; broken symlinks; sparse files; hardlink farms) and **fails the build on a >10% regression** against the recorded baseline. Performance is a test, not an aspiration.

## 12. Error handling, logging, diagnostics

- One error type per crate, `thiserror`-derived, converging on `duet_types::Error` with the taxonomy from §9.3. Errors carry the `VPath` and the syscall/errno when relevant.
- **User-facing errors are actionable**: what failed, on which file, why, and what the user can do (retry / skip / elevate / open location). No raw errno strings in the primary line.
- `tracing` with per-crate filters; `--log-level` and `DUET_LOG`; a ring buffer of the last N events dumped alongside any crash.
- Panics in non-UI tasks are caught at the task boundary, reported as job failures, and never kill the app. A panic on the UI thread writes a crash file and attempts a session-state save before dying.
- Opt-in, off-by-default crash reporting; nothing leaves the machine without an explicit choice (§13).

## 13. Security and privacy

| Concern | Decision |
|---|---|
| Plugin sandbox | WASM, no ambient authority, host-mediated handles, resource limits (§9.9) |
| Path traversal | All local operations use `*at` syscalls against an open dirfd; no re-resolution of user-visible paths at execute time (TOCTOU) |
| Symlinks | Never followed implicitly during recursive delete or recursive chmod; explicit "follow symlinks" is per-operation and off by default |
| Archive extraction | Zip-slip and absolute-path members rejected; symlink members rejected unless explicitly enabled; decompression-ratio bomb detection with a configurable ceiling |
| Elevation | Narrow polkit helper with a fixed verb set and argument validation; never run the GUI as root; refuse to start as root with a warning unless `--i-know-what-im-doing` |
| Secrets | Remote credentials in the Secret Service keyring; never in config files; memory zeroised on drop |
| Remote trust | Strict SSH host-key checking with an explicit TOFU prompt; TLS certificate validation on by default with per-profile pinning |
| Thumbnailers | Untrusted decoders run as subprocesses with a timeout, memory cap, and no network |
| Telemetry | None by default. Any future opt-in telemetry must be documented, local-first, and inspectable |

## 14. Test strategy

| Level | Content | Gate |
|---|---|---|
| **14.1 Unit** | Path arithmetic, mask compilation, conflict-policy resolution, sort comparators, config migration. Property tests (`proptest`) for path round-tripping and mask matching. | Every PR |
| **14.2 VFS conformance** | One suite, run against *every* backend (local, each archive, each remote, plus a fault-injecting wrapper). A backend is "done" when it passes. Includes capability-honesty checks: a backend claiming `ATOMIC_REPLACE` must actually provide it. | Per backend, G4 |
| **14.3 UI integration** | Headless via GPUI's `TestAppContext`: keymap dispatch, focus and selection semantics, dialog keyboard-completeness, panel diff application. | Every PR |
| **14.4 Data-safety (release gate)** | Crash injection at every step boundary of copy/move/delete (SIGKILL harness); ENOSPC via a small loop device; EACCES; disconnect mid-transfer; `dm-flakey` for I/O errors. **Assertion: for every injection point, either the source is intact and the destination is absent/partial-and-marked, or the destination is complete. Never anything else.** | G3, and every release |
| **14.5 Platform matrix** | GNOME/Wayland, KDE/Wayland, sway, X11+i3, XFCE. HiDPI 1×/1.5×/2×, mixed-DPI multi-monitor. Filesystems: ext4, btrfs (reflink), xfs, tmpfs, exFAT, NTFS3, NFS, SMB, sshfs, FUSE. | G4, G5 |
| **14.6 Performance** | The §11 suite; regression-gated. | Every PR (fast subset), nightly (full) |
| **14.7 Fuzzing** | `cargo-fuzz` on archive parsers, config parsers, encoding detection, and the plugin WIT boundary. Continuous. | Nightly |
| **14.8 Manual/exploratory** | A scripted acceptance pass driven by the FR table; plus a "TC muscle-memory" session with a P1-persona tester who is *not* the author. | G3, G4, G5 |

## 15. Build, packaging, distribution

- CI: build + clippy + fmt + test on stable, MSRV pinned to the GPUI requirement; nightly for miri on the unsafe surface in `duet-vfs`.
- Artifacts: Flatpak (primary — solves the "no GTK/Qt runtime" story cleanly, with `--filesystem=host` documented honestly), AppImage, `.deb`, `.rpm`, AUR `PKGBUILD`, and a static-ish tarball.
- Flatpak caveat to resolve in Phase 10: a sandboxed file manager is in tension with being a file manager. Decide (T-10.2.3) between full `--filesystem=host` with a clear permission note, or portal-mediated access with a documented UX cost.
- Reproducible-ish builds, `SOURCE_DATE_EPOCH` honoured; release artifacts signed; `SBOM` generated via `cargo-cyclonedx`.
- Docs site built from the repo: user manual, keymap reference, plugin author guide, and the config schema.

## 16. Milestones

| ID | Milestone | Contents | Gate |
|---|---|---|---|
| **M0** | Feasibility | Phase 0 spikes; ADR-001 confirmed or revised | G0 |
| **M1** | Walking skeleton | Two panels, local VFS, listing, navigation, keymap, palette; no operations | — |
| **M2** | Alpha | All FR-NAV/SEL P0, FR-OPS P0, viewer, search, trash, clipboard; data-safety suite green | G3 |
| **M3** | Beta | Archives, remote backends, plugin system, multi-rename, sync, thumbnails | G4 |
| **M4** | RC | Platform matrix clean, performance targets met, docs complete, packaging built | — |
| **M5** | 1.0 | All P0/P1; announcement; plugin registry live | G5 |

**Effort estimate.** `task.md` sums to roughly **520–620 ideal engineer-days** for M5. As a solo side project at ~8 productive hours/week that is unrealistic as scoped; §16.1 therefore defines a defensible reduced scope.

### 16.1 Reduced-scope path (recommended for a solo effort)

If this is one person's evenings, cut to **M2 + archives-read-only** and ship that as 1.0: Phases 0–5 plus tasks 6.1–6.3, dropping remote backends, the plugin system, sync, and the editor to post-1.0. That is roughly **180–220 ideal days**, it is a genuinely useful product on day one, and — importantly — it is the subset where GPUI's advantages are most visible and its gaps (§7.4) least exposed. The plugin system in particular should not be built until there is a user base asking for it; a plugin API with no plugins is a maintenance liability with no payoff.

## 17. Open questions

| ID | Question | Needed by | Owner |
|---|---|---|---|
| OQ-1 | Does GPUI expose enough clipboard control for `text/uri-list`, or must we own the data device? | G0 | S-2 |
| OQ-2 | Cross-application DnD on Wayland — feasible within GPUI, or protocol-level work? | G0 | S-3 |
| OQ-3 | `gpui-component`'s Table at 1M rows — does it hold, and is its delegate API workable for SoA data? | G0 | S-1 |
| OQ-4 | AT-SPI accessibility: is AccessKit integration tractable, and should we upstream it? | G4 | — |
| OQ-5 | Own the remote-protocol stack, or lean on `opendal` for breadth and accept its abstraction? | G2 | — |
| OQ-6 | Flatpak permission posture (§15) | G5 | — |
| OQ-7 | Native/Wine plugin bridge — worth it, or does it poison the security model? | Post-1.0 | — |
| OQ-8 | Name and visual identity | G5 | — |

## Appendix A — Full default keymap

*(To be completed in T-1.4.2 against Total Commander 11 and Double Commander 1.1 as references. The extract in §9.4 is the committed subset; the appendix must enumerate every binding, its TC provenance, and any deliberate deviation with a rationale — deviations are the thing P1 users will file bugs about, so each one needs a written defence.)*

## Appendix B — Glossary

| Term | Meaning |
|---|---|
| OFM | Orthodox File Manager — the two-panel, keyboard-driven, command-line-integrated lineage from Norton Commander |
| Source / target panel | The active panel is the source of operations; the inactive one is the default destination |
| VFS | Virtual filesystem — a backend addressable through the same interface as local disk |
| WCX / WFX / WDX / WLX | Total Commander's packer / filesystem / content / lister plugin classes |
| Reflink | Copy-on-write clone of file extents (`FICLONE`), instant and space-free on btrfs/xfs |
| Journal | The append-only record of an operation's intended and completed steps, enabling crash recovery |

## Appendix C — ADR index

| ADR | Title | Status |
|---|---|---|
| ADR-001 | GPUI + gpui-component for the shell | Accepted, conditional on G0 |
| ADR-002 | UI-framework-agnostic core | Accepted |
| ADR-003 | Pin GPUI, isolate churn behind a compat shim | Accepted |
| ADR-004 | WASM component plugins over native `.so` | Accepted |
| ADR-005 | Own the trash/clipboard/mount integration rather than depend on GIO | Proposed |
| ADR-006 | `opendal` vs. hand-rolled remote backends | Open (OQ-5) |
