# G2 Gate Report — Architecture & Detailed Design

| Field | Value |
|---|---|
| Document ID | DUET-G2-001 |
| Gate | G2 (`design.md` approval gates table) |
| Consolidates | T-2.1.1, T-2.1.2, T-2.2.1, T-2.2.2, T-2.3.1, T-2.3.2, T-2.4.1, T-2.5.1, T-2.6.1, T-2.7.1 |
| Date | 2026-08-11 |
| Status | Pass |

## Decision: **Pass**

Gate G2's exit criteria (`task.md`, Phase 2 header): *"all interfaces below compile as trait/type skeletons with `todo!()` bodies; ADRs written; no interface question left to implementation time."*

| Clause | Status |
|---|---|
| All interfaces compile as trait/type skeletons | **Met.** `cargo build --workspace --all-targets` is green across all 12 crates + `duet-privileged` + `plugins-sdk`, with real (not stubbed-empty) type/trait shapes in every crate a Phase 2 task targeted. Where genuine runtime logic is out of scope for this phase, bodies are `todo!()` — but every field, variant, and method signature is real. |
| ADRs written | **Met.** ADR-001 through ADR-006 are all finalized (see below) — three (001–003) were already written pre-G0/G1; three (004–006) were open or proposed and are closed out in this gate. |
| No interface question left to implementation time | **Met, with one explicit exception carried forward on purpose.** See "What G2 did not resolve" below — NFR-05's 120Hz validation, which is a *measurement* gap from G0, not an interface design gap, and was already correctly scoped to T-4.2.1 rather than something G2 could resolve. |

## ADR disposition

| ADR | Decision | Basis |
|---|---|---|
| ADR-001 (GPUI + gpui-component) | Accepted, unconditionally | G0 (already closed) |
| ADR-002 (UI-framework-agnostic core) | Accepted | Enforced continuously — every Phase 2 crate passes `scripts/check-gpui-isolation.sh` |
| ADR-003 (pin GPUI, compat shim) | Accepted | G0 (already closed) |
| ADR-004 (WASM plugins over native `.so`) | **Finalized this gate** | S-7's spike: 60–130x margin on call overhead, confirmed epoch interruption. T-2.6.1's WIT interfaces + validated host↔guest round trip give this decision a working reference implementation, not just a paper argument. |
| ADR-005 (own trash/clipboard/mount, not GIO) | **Finalized this gate** | S-2's clipboard spike already forced direct protocol ownership (GPUI can't carry custom MIME types); extending that ownership to trash/mounts avoids a second heavy dependency for a problem already solved at the protocol level. |
| ADR-006 / OQ-5 (`opendal` vs. hand-rolled) | **Decided this gate** | Hybrid: `opendal` for S3/WebDAV/FTP (breadth-favorable, P1, no differentiated reason to reimplement), hand-rolled for SFTP (P1, persona P2's primary workflow, needs tighter control) and SMB (no mature `opendal` backend exists). `opendal` is an implementation detail behind `duet-vfs`'s `FileSystem` trait, never exposed directly. |

## What each task produced

**T-2.1.1 / T-2.1.2** — the Cargo workspace itself: 12 crates matching `design.md` §8.1's layout exactly, `duet-privileged` helper, `plugins-sdk`. CI (fmt/clippy/build+test/MSRV/ADR-002 lint) as separate jobs. The gpui-isolation lint (`scripts/check-gpui-isolation.sh`) was verified to actually catch a deliberate violation, not just assumed to work.

**T-2.2.1 (`duet-types`)** — `VPath`/`MountId`/`EntryId`/`Metadata`/`Caps`/`MetaPatch`/error taxonomy. `VPath` round-trips through 2048 proptest cases per property, including nested-archive paths (`zip:zip:file://...!/...!/...`) and reserved-character filenames, via a hand-rolled percent-encoding scheme (`pct.rs`) and a self-describing `MountId` design (`Root{scheme,authority}`/`Nested{scheme,parent}` rather than an opaque integer handle — necessary because `VPath` must `Display`/`FromStr` round-trip without a live mount table to resolve against).

**T-2.2.2 (`FileSystem` trait)** — the full trait per `design.md` §9.1, object-safe (verified: constructs as both `Box<dyn FileSystem>` and `Arc<dyn FileSystem>`, matching the mount table's actual shape), `NullFs` proving the trait is implementable end-to-end. Surfaced and fixed a real design issue: `VfsError` at ~144 bytes tripped clippy's `result_large_err` on every trait method, since `Result<T>` returned it by value — fixed at the source by boxing the crate-wide alias (`Box<VfsError>`), which every consumer now uses.

**T-2.3.1 / T-2.3.2 (operation engine + crash-safety sketch)** — `Plan`/`Step`/`Job`/`JobEvent`/`ConflictPolicy`/`Journal` in `duet-ops`, with a real serialize/deserialize round-trip test for a hand-written 3-file-copy plan. `docs/crash-safety.md`: 45 rows across all 8 `Step` kinds, each citing the specific `JournalRecord` variant a recovery reader observes and naming a future T-10.2.1 test — notably, it distinguishes syscalls that are *genuinely* atomic (no real partial state possible) from `Step::Remove{Recursive}`, which is the one step kind where a truly partial on-disk state is possible and expected, rather than treating every row as equally uncertain.

**T-2.4.1 (command registry, keymap, predicates)** — `Command`/`CommandRegistry`, a hand-rolled predicate parser with correct `&&`/`||` precedence and negation (73 tests, including a dedicated malformed-input corpus), `KeymapFile` matching `docs/config-schema.md` exactly with real load-time conflict detection, and a palette index using the same fuzzy-matching crate family as FR-NAV-13's quick-search for consistency. Built before `duet-types` existed on its source branch; reconciliation check at this gate confirmed its local error types are correctly domain-local (parse/registration errors, not VFS errors) rather than a gap needing a fix.

**T-2.5.1 (panel model)** — `EntryStore` (SoA layout, byte-budget arithmetic documented against the ≤96B+name target), `DirectoryModel`, `DirEntryDiff` (insert/remove/update/reorder/reset). Ported S-1's spike-validated approach using real `duet_types` types. **A genuine bug was found and fixed during this gate's consolidation**: `NameArena`'s slab-boundary tracking conflated a fixed breakpoint marker with a live-growing counter in the same storage slot, causing incorrect slab lookups (a subtract-overflow panic, then a silent wrong-slab read) once more than one slab flush occurred. Caught by the crate's own 10,000-entry multi-boundary test; fixed by making every breakpoint fixed-once-written, never mutated in place.

**T-2.6.1 (WIT interfaces)** — all five plugin-class worlds (`content`, `packer`, `fs`, `viewer`, `command`) plus `host`/`types`, in `plugins-sdk/wit/`. Validated, not just written: `plugins-sdk/examples/build.sh` builds and componentizes a stub guest for each world, and `host-check` performs a real `wasmtime::component::bindgen!`-generated host↔guest call through `content-plugin-world`, including the `open-granted` handle-based resource path — the part of the design that actually matters for FR-PLUG-03's capability model.

## A note on process for this gate

Three of Phase 2's tasks (T-2.2.2, T-2.5.1, T-2.6.1) were interrupted mid-work by a session usage limit, after their subagents had left uncommitted progress in their worktrees. Rather than re-running the same subagent prompts (likely to fail the same way immediately), the work was picked up directly: two of the three had real, non-trivial bugs waiting in their untested code (the `result_large_err` design issue in T-2.2.2, the slab-boundary bug in T-2.5.1) that only surfaced once the test suites were actually run to completion. Both are documented above and in their respective commits, not glossed over. This gate's own consolidation additionally re-ran the full verification suite (build, test, clippy, fmt, isolation lint) independently rather than trusting each task's self-report at face value — this caught nothing further, but the T-2.2.2/T-2.5.1 experience earlier in this phase is the reason that re-verification happened at all rather than being skipped as redundant.

## What G2 did not resolve

- **NFR-05's 120Hz frame-time validation** (carried from G0): still unmeasured, no genuine 120Hz display existed in any environment used for this project so far. This is explicitly not a G2 concern — it's a hardware-dependent measurement task correctly owned by T-4.2.1, not an interface design question G2 could have closed.
- **Keymap verification** (carried from G1): 49 `inferred`/`uncertain` rows in `docs/keymap-tc.csv` still need a human with real TC 11 access to spot-check. Unchanged since G1; not a Phase 2 concern.
- **P1/P2 acceptance sketches** (carried from G1): still not written, still correctly deferred to T-10.4.1.

None of these block Phase 3. The interface surface Phase 3 (`duet-vfs local backend`, `duet-index` listing/watching, cross-cutting core) needs to build against is now real and compiling.
