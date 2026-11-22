# Crash-safety proof sketch (T-2.3.2) and its verification (T-10.2.1)

**Traces:** FR-OPS-07, NFR-08.
**Verified by:** `crates/duet-ops/tests/data_safety/` -- see "How the suite verifies this table" at the end.
**Types referenced:** `crates/duet-ops/src/step.rs` (`Step`, `StepKind`), `crates/duet-ops/src/journal.rs` (`JournalRecord`, `StepOutcome`).

## Scope and method

FR-OPS-07 states the guarantee this document exists to justify:

> An interrupted operation leaves either the old file intact or a clearly-marked partial file, never a silently truncated destination. Journal permits resume.

For every [`Step`](../crates/duet-ops/src/step.rs) kind defined in T-2.3.1, this document enumerates every point in that step's execution where a `SIGKILL` (or an equivalent hard interruption — power loss, `OOM-killer`, `kill -9`) can land, and states the on-disk invariant that must hold immediately afterward. Each row names the test in the data-safety suite (**T-10.2.1**, task.md: "SIGKILL injection at every step boundary... Every injection point satisfies the §9.3 invariant. Release gate: 100%, no exceptions, no 'flaky' annotations") that proves that invariant by actually injecting a `SIGKILL` at that point and inspecting the result. The names were fixed when this table was written (T-2.3.2), before the suite existed, so that there would be no ambiguity later about what "every step × every interruption point" means; `tests/data_safety/sigkill_matrix.rs` now implements each one under exactly that name, and `tests/data_safety/checklist.rs` fails if the table and the tests ever disagree.

**Mechanism common to every step**, referenced by every invariant below rather than repeated in each row:

1. Before a step has *any* observable side effect, the executor appends a [`JournalRecord::Intent`] for it and the journal's `append()` call does not return until that record is `fsync`'d (design.md §9.3; `Journal::append`'s doc comment in `journal.rs`). A crash before this fsync completes is indistinguishable, on restart, from the step never having been selected for execution at all — there is nothing to recover because nothing durable was promised.
2. Any step whose action produces or replaces a destination *file* (`CopyFile`, `Reflink`) never writes to the final destination path directly. It writes to a sibling `.duet-partial-<rand>` path and only `rename(2)`s that sibling onto the final destination once the write is complete (and, if `PlanOptions::verify` is set, verified). This is what makes "clearly-marked partial file" a literal filename convention rather than a vague promise.
3. After a step's action completes, the executor appends a [`JournalRecord::Completion`] for it, again fsync'd before returning.
4. Linux VFS-level operations this document treats as atomic — `rename(2)`/`renameat2(2)`, `mkdir(2)`/`mkdirat(2)`, `link(2)`/`linkat(2)`, `unlink(2)`/`rmdir(2)` on a single non-directory-tree target — genuinely are: the kernel never exposes a half-applied state of *that single syscall* to any observer, including one that crashes and remounts immediately after. A row labelled "mid-`<syscall>`" therefore does not describe a real third state; it exists in the table to make that non-existence an explicit, tested claim rather than an unstated assumption. (Directory-entry *durability*, as opposed to atomicity, is what `fsync` on the containing directory buys, and is covered separately where it matters — see `CreateDir` and `Rename`.)
5. `Step::Remove { mode: RemoveMode::Recursive, .. }` is the sole exception to point 4: a recursive delete is a sequence of many individual atomic removals, not one atomic operation, so a genuinely partial state (some descendants gone, others not) is possible and expected — see its own section.

Every table row below cites the specific [`JournalRecord`]/[`StepOutcome`] variant a recovery reader (`JournalReader::scan`, T-5.1.2) observes at that point, since that is what the recovery reader actually has to work with — not the on-disk file state directly, which it cannot always distinguish without the journal's help (a `.duet-partial-*` file could be genuinely mid-write or could be a fully-written partial waiting on a verify step; the journal, not a `stat()`, is what tells them apart).

---

## `CopyFile`

Buffered/`copy_file_range` copy from `source` to `dest`, staged through `.duet-partial-<rand>`.

| # | Interruption point | Journal state observed on recovery | Invariant | Verifying test (T-10.2.1) |
|---|---|---|---|---|
| 1 | Before the `Intent` record is fsync'd | No record for this step at all | `dest` does not exist (or, if it existed before this job, is unchanged); no `.duet-partial-*` file exists for this step. Resuming simply re-plans this step from scratch. | `copyfile_sigkill_before_intent_leaves_no_trace` |
| 2 | After `Intent` fsync'd, before the partial file is created | `Intent` present, no `Completion` | `dest` unchanged from before the job; no partial file yet. Recovery re-attempts the step from position zero. | `copyfile_sigkill_after_intent_before_partial_created_dest_untouched` |
| 3 | Mid-write (partial file created, partially written) | `Intent` present (with `partial_name`), no `Completion` | The pre-existing `dest` (if any) is byte-for-byte identical to before the job — it was never touched. `.duet-partial-<rand>` exists, is non-empty, and is shorter than or equal to the planned size; it is never mistaken for a valid destination (wrong name, and `Intent.partial_name` confirms it's this step's orphan). | `copyfile_sigkill_mid_write_leaves_source_intact_and_partial_marked` |
| 4 | After the write completes and is `fsync`'d, before `rename(2)` is issued | `Intent` present, no `Completion` | Same as #3: `dest` untouched, `.duet-partial-<rand>` now holds the *complete* copied content (this is the one state where the partial file is actually valid data, just not yet named correctly). Recovery can offer "resume" as a rename-only step, not a re-copy. | `copyfile_sigkill_after_write_before_rename_partial_is_complete_and_resumable` |
| 5 | Mid-`rename(2)` (kernel-atomic; see method point 4) | `Intent` present, no `Completion` (recovery cannot yet tell, from the journal alone, which side of the rename it landed on) | `dest` is in **exactly one** of two states: the pre-job file (rename didn't happen) or the fully-copied content under the final name (rename happened) — never a truncated or mixed-content file, and never both a `.duet-partial-*` orphan *and* a renamed `dest` simultaneously. Recovery's job here is to `stat()` both paths and infer which; the *filesystem's* job (already discharged by atomicity) is to guarantee there is a clean answer to find. | `copyfile_sigkill_mid_rename_dest_is_old_or_new_never_mixed` |
| 6 | After `rename(2)` returns, before `Completion` is fsync'd | `Intent` present, no `Completion`, but `dest` already has the final content on disk | `dest` holds the complete, correctly-named copy. The *journal* is stale (doesn't yet know the step succeeded), but the *data* is safe — recovery re-derives success by noticing `dest` exists with the planned size/hash and treats the step as already done rather than re-copying. | `copyfile_sigkill_after_rename_before_completion_record_dest_correct_journal_catches_up` |
| 7 | After `Completion` is fsync'd | `Intent` + `Completion { Succeeded }` | Fully durable terminal state; recovery has nothing to do for this step. | `copyfile_sigkill_after_completion_record_step_is_stable` |

## `Reflink`

`ioctl(FICLONE)` copy-on-write clone from `source` to `dest`, same staging convention as `CopyFile` (method point 2 applies uniformly).

| # | Interruption point | Journal state observed on recovery | Invariant | Verifying test (T-10.2.1) |
|---|---|---|---|---|
| 1 | Before `Intent` fsync'd | No record | Identical to `CopyFile` #1. | `reflink_sigkill_before_intent_leaves_no_trace` |
| 2 | After `Intent` fsync'd, before the partial file is created | `Intent`, no `Completion` | Identical to `CopyFile` #2. | `reflink_sigkill_after_intent_before_partial_created_dest_untouched` |
| 3 | Mid-`ioctl(FICLONE)` (kernel-atomic: a reflink either clones the whole extent map or not at all — there is no partial-clone state, unlike a buffered write) | `Intent`, no `Completion` | The partial-named file is either absent/zero-length (clone not yet applied) or a complete, correct clone (clone applied) — there is no analogue of `CopyFile`'s "mid-write, partially written" row, because `FICLONE` has no streaming phase. `dest` (pre-job) untouched either way. | `reflink_sigkill_mid_ficlone_partial_is_absent_or_complete_never_truncated` |
| 4 | After `FICLONE` returns, before `rename(2)` | `Intent`, no `Completion` | `.duet-partial-<rand>` holds the complete reflinked content; resumable as rename-only, same as `CopyFile` #4. | `reflink_sigkill_after_ficlone_before_rename_partial_is_complete_and_resumable` |
| 5 | Mid-`rename(2)` | `Intent`, no `Completion` | Identical reasoning to `CopyFile` #5. | `reflink_sigkill_mid_rename_dest_is_old_or_new_never_mixed` |
| 6 | After `rename(2)`, before `Completion` fsync'd | `Intent`, no `Completion`, `dest` already correct | Identical reasoning to `CopyFile` #6. | `reflink_sigkill_after_rename_before_completion_record_dest_correct_journal_catches_up` |
| 7 | After `Completion` fsync'd | `Intent` + `Completion { Succeeded }` | Fully durable. Additionally: `dest`'s extents are shared with `source`'s (verified via `filefrag`/`FIEMAP`, not just content equality), since a reflink that silently degraded to a full copy without updating `CopyOutcome.reflinked` would be a correctness bug the crash-safety suite should also catch. | `reflink_sigkill_after_completion_record_step_is_stable_and_still_reflinked` |

## `CreateDir`

`mkdirat` at `dest`, non-recursive.

| # | Interruption point | Journal state observed | Invariant | Verifying test (T-10.2.1) |
|---|---|---|---|---|
| 1 | Before `Intent` fsync'd | No record | `dest` does not exist. | `createdir_sigkill_before_intent_leaves_no_trace` |
| 2 | After `Intent` fsync'd, before `mkdirat` is issued | `Intent`, no `Completion` | `dest` does not exist yet; recovery re-attempts the same `mkdirat`. | `createdir_sigkill_after_intent_before_mkdir_dest_absent` |
| 3 | Mid-`mkdirat` (kernel-atomic) | `Intent`, no `Completion` | `dest` is either fully absent or fully present as an empty directory with the planned mode bits — never a directory entry pointing at an inconsistent inode. | `createdir_sigkill_mid_mkdir_dest_absent_or_fully_present` |
| 4 | After `mkdirat` returns, before `Completion` fsync'd | `Intent`, no `Completion`, but `dest` exists on disk | `dest` exists as a valid, empty directory; recovery notices it already exists and treats the step as done rather than erroring on `EEXIST` or, worse, retrying destructively. | `createdir_sigkill_after_mkdir_before_completion_record_recovery_treats_as_done` |
| 5 | After `Completion` fsync'd | `Intent` + `Completion { Succeeded }` | Fully durable; steps that write *inside* `dest` (which the planner ordered after this one) are safe to (re-)run. | `createdir_sigkill_after_completion_record_step_is_stable` |

## `Rename`

Same-filesystem move via `renameat2`.

| # | Interruption point | Journal state observed | Invariant | Verifying test (T-10.2.1) |
|---|---|---|---|---|
| 1 | Before `Intent` fsync'd | No record | `source` still exists at its original path; `dest` unaffected. | `rename_sigkill_before_intent_leaves_no_trace` |
| 2 | After `Intent` fsync'd, before `renameat2` is issued | `Intent`, no `Completion` | `source` still present at the original path. | `rename_sigkill_after_intent_before_renameat2_source_untouched` |
| 3 | Mid-`renameat2` (kernel-atomic) | `Intent`, no `Completion` | The entry exists at **exactly one** of `source` or `dest` — never both, never neither, never a partially-visible link at either name. This is the single most safety-critical atomicity claim in the whole engine, since unlike `CopyFile` there is no staged temp file cushioning the transition — `renameat2` *is* the whole operation. | `rename_sigkill_mid_renameat2_entry_at_exactly_one_of_source_or_dest` |
| 4 | After `renameat2` returns, before `Completion` fsync'd | `Intent`, no `Completion`, but the entry is already at `dest` | Recovery `stat()`s `source` (absent) and `dest` (present) and infers success rather than re-attempting a rename that would now fail `ENOENT` on `source`. | `rename_sigkill_after_renameat2_before_completion_record_recovery_infers_success` |
| 5 | After `Completion` fsync'd | `Intent` + `Completion { Succeeded }` | Fully durable. | `rename_sigkill_after_completion_record_step_is_stable` |

## `Link`

Hardlink `dest` to `source` via `linkat`.

| # | Interruption point | Journal state observed | Invariant | Verifying test (T-10.2.1) |
|---|---|---|---|---|
| 1 | Before `Intent` fsync'd | No record | `dest` does not exist; `source`'s link count unchanged. | `link_sigkill_before_intent_leaves_no_trace` |
| 2 | After `Intent` fsync'd, before `linkat` is issued | `Intent`, no `Completion` | Same as #1. | `link_sigkill_after_intent_before_linkat_dest_absent` |
| 3 | Mid-`linkat` (kernel-atomic) | `Intent`, no `Completion` | `dest` is either fully absent or fully present pointing at `source`'s inode with an incremented link count — never a dangling or partially-linked entry. | `link_sigkill_mid_linkat_dest_absent_or_fully_linked` |
| 4 | After `linkat` returns, before `Completion` fsync'd | `Intent`, no `Completion`, `dest` already linked | Recovery notices `dest` exists with the same `(dev, ino)` as `source` and treats the step as done, rather than attempting a second `linkat` that would fail `EEXIST` or, worse, double-counting the link in the job's hardlink-graph accounting. | `link_sigkill_after_linkat_before_completion_record_recovery_treats_as_done` |
| 5 | After `Completion` fsync'd | `Intent` + `Completion { Succeeded }` | Fully durable. | `link_sigkill_after_completion_record_step_is_stable` |

## `SetMeta`

Applies a `MetaPatch` to `target`, in the order design.md §9.3 specifies: mode → xattrs/ACL/SELinux label → timestamps last. Unlike the previous kinds, this step is **not** a single syscall — it is genuinely multi-phase, so "mid-apply" is a real, not merely notional, state.

| # | Interruption point | Journal state observed | Invariant | Verifying test (T-10.2.1) |
|---|---|---|---|---|
| 1 | Before `Intent` fsync'd | No record | `target`'s metadata is completely unchanged from before the step. | `setmeta_sigkill_before_intent_leaves_no_trace` |
| 2 | After `Intent` fsync'd, before any field is applied | `Intent`, no `Completion` | Same as #1. | `setmeta_sigkill_after_intent_before_any_field_applied_target_untouched` |
| 3 | Mid-apply (e.g. mode already set, xattrs/ACL/timestamps not yet) | `Intent`, no `Completion` | **Content is never touched by `SetMeta`** — a crash here can leave `target`'s metadata in a state that is neither "old" nor "new" (some fields from each), but never touches file data, and each *individual* field-set syscall (`chmod`, `setxattr`, `utimensat`, ...) is itself atomic, so no single field is ever half-written. Recovery's job is to re-apply the *whole* patch idempotently — every field in `MetaPatch` is a "set to exactly this value" operation, not a delta, so re-running it from `Intent.step` is always correct regardless of which prefix of fields had already landed. | `setmeta_sigkill_mid_apply_content_untouched_and_reapply_is_idempotent` |
| 4 | After every field is applied, before `Completion` fsync'd | `Intent`, no `Completion`, `target`'s metadata already fully matches the patch | Recovery re-applies the patch anyway (idempotent, per #3) rather than needing to detect completion precisely — cheaper to guarantee correctness this way than to diff current-vs-patched state. | `setmeta_sigkill_after_all_fields_applied_before_completion_record_reapply_is_harmless` |
| 5 | After `Completion` fsync'd | `Intent` + `Completion { Succeeded }` | Fully durable; recovery skips this step entirely. | `setmeta_sigkill_after_completion_record_step_is_stable` |

## `Remove`

Deletes are journaled **before** execution regardless of `RemoveMode` (design.md §9.3: "so the undo stack has something to work from for trash operations") — this is method point 1 applied without exception, which matters more here than for any other step kind because `Remove` is the one kind that destroys data the job did not itself create.

| # | Interruption point | Journal state observed | Invariant | Verifying test (T-10.2.1) |
|---|---|---|---|---|
| 1 | Before `Intent` fsync'd | No record | `target` (and, for `Recursive`, its entire subtree) is completely untouched. Since nothing durable was promised, the undo stack has nothing to act on, correctly — the delete hasn't happened. | `remove_sigkill_before_intent_leaves_no_trace` |
| 2 | After `Intent` fsync'd, before removal begins | `Intent`, no `Completion` | `target` still fully present. The undo stack *can* already see the `Intent` and knows "a delete of this path was about to happen," which is exactly the FR-OPS-14 guarantee — undo data exists from the moment intent is durable, not from the moment the delete finishes. | `remove_sigkill_after_intent_before_removal_target_untouched_undo_data_present` |
| 3a | Mid-removal, `mode: File` or `EmptyDir` (kernel-atomic `unlinkat`/`rmdir`) | `Intent`, no `Completion` | `target` is either fully present or fully gone — never a dangling directory entry. | `remove_sigkill_mid_removal_file_or_emptydir_target_absent_or_fully_present` |
| 3b | Mid-removal, `mode: Recursive` (genuinely non-atomic: a sequence of individual `unlinkat`/`rmdir` calls walking the subtree) | `Intent`, no `Completion` | Some descendants are gone, others remain — this is the one legitimate "partially applied" state anywhere in this document. The invariant is **not** atomicity (impossible for a multi-entry recursive delete without a whole-subtree transaction the filesystem doesn't offer) but *boundedness and resumability*: every entry that is gone was genuinely supposed to go (no over-deletion outside the intended subtree — see also the symlink-escape test in T-3.1.3/T-5.1.8), and the remaining entries are exactly the "remaining work" a resumed delete re-walks and finishes; no entry is silently skipped and left behind permanently. | `remove_sigkill_mid_recursive_removal_partial_is_bounded_and_resumable_no_escape` |
| 4 | After removal completes, before `Completion` fsync'd | `Intent`, no `Completion`, `target` already fully gone | Recovery `stat()`s `target` (`ENOENT`), infers success, and does not attempt to remove an already-absent path (which would otherwise surface as a spurious error). | `remove_sigkill_after_removal_before_completion_record_recovery_infers_success` |
| 5 | After `Completion` fsync'd | `Intent` + `Completion { Succeeded }` | Fully durable. | `remove_sigkill_after_completion_record_step_is_stable` |

## `Verify`

Read-only content/size comparison of `source` against `dest`. Never mutates either path — this is the one step kind whose crash-safety story is "nothing to prove about *this* step's own output," but a table row is still owed for two reasons: (a) T-2.3.2's AC asks for every step kind without exception, and (b) `Verify` gates whether a preceding cross-device move's `Remove` (of `source`) is allowed to run at all, so its own crash behaviour still matters to the larger operation's safety even though it writes nothing itself.

| # | Interruption point | Journal state observed | Invariant | Verifying test (T-10.2.1) |
|---|---|---|---|---|
| 1 | Before `Intent` fsync'd | No record | Neither `source` nor `dest` touched (they were already fully written by the preceding `CopyFile`/`Reflink` step, which has its own `Completion` record). | `verify_sigkill_before_intent_leaves_no_trace` |
| 2 | After `Intent` fsync'd, before hashing/comparison begins | `Intent`, no `Completion` | Same as #1; recovery simply re-runs the comparison. | `verify_sigkill_after_intent_before_hash_no_side_effect` |
| 3 | Mid-hash-computation (reading `dest`, computing BLAKE3 or comparing sizes) | `Intent`, no `Completion` | Purely a read; `source`/`dest` unchanged regardless of how far the read got. Recovery re-runs the comparison from scratch — there is no partial-hash state worth resuming, since re-hashing is cheap relative to the copy it's verifying. | `verify_sigkill_mid_hash_computation_no_side_effect_reruns_cleanly` |
| 4 | After the comparison result is known, before `Completion` fsync'd | `Intent`, no `Completion` | The comparison result itself is lost (it was never durable — only the fact that `Intent` exists is), so recovery treats this exactly like #3: re-run the comparison. This is deliberately conservative (a few extra seconds of re-hashing) rather than risk trusting an unwritten in-memory result across a crash. | `verify_sigkill_after_result_computed_before_completion_record_reruns_conservatively` |
| 5 | After `Completion` fsync'd | `Intent` + `Completion { Succeeded }` (or `Completion { Failed(_) }`, if verification found a mismatch — a real, expected outcome, not a bug) | Fully durable. If the result was `Failed`, the preceding source (for a cross-device move) is **not** unlinked — this is exactly design.md §9.3's "Never unlink before the destination is durable [and verified]" — and the job surfaces the mismatch through `JobEvent::StepFailed`/`JobReport.errors` rather than silently proceeding. | `verify_sigkill_after_completion_record_step_is_stable_and_failed_verify_blocks_source_unlink` |

## `Symlink`

Added after T-2.3.1 (T-5.2.7). `symlinkat` of `link_path` storing `target` verbatim; strict protocol (not redo-safe: a redo of a finished symlink collides with it).

| # | Interruption point | Journal state observed | Invariant | Verifying test (T-10.2.1) |
|---|---|---|---|---|
| 1 | Before `Intent` fsync'd | No record | `link_path` does not exist. | `symlink_sigkill_before_intent_leaves_no_trace` |
| 2 | After `Intent` fsync'd, before `symlinkat` is issued | `Intent`, no `Completion` | `link_path` does not exist. | `symlink_sigkill_after_intent_before_symlinkat_link_absent` |
| 3 | After `symlinkat` returns, before `Completion` fsync'd | `Intent`, no `Completion`, link already present | Recovery reads the link, finds exactly the planned target, and treats the step as done rather than colliding with its own earlier work. (Recognised for an absolute target only: the VFS reports a symlink's target as a normalised absolute path, so a relative one cannot be compared and is redone -- which resolves as a conflict under the job's policy, `Skip` by default. No data is involved either way.) | `symlink_sigkill_after_symlinkat_before_completion_record_recovery_treats_as_done` |
| 4 | After `Completion` fsync'd | `Intent` + `Completion { Succeeded }` | Fully durable. | `symlink_sigkill_after_completion_record_step_is_stable` |

("Mid-`symlinkat`" is kernel-atomic exactly like `linkat` and has no row of its own; the `Link` table's row 3 exercises the same claim.)

## `WriteTrashInfo`

Added after T-2.3.1 (T-5.3.1). Writes a freedesktop `.trashinfo` sidecar through `FileSystem::open_write` (staged sibling, fsync, rename-no-replace). Always followed by a `Rename` gated on it -- the content moves into the trash only once its sidecar is durable, because an orphaned sidecar is harmless and an orphaned trashed file with no sidecar cannot be restored.

| # | Interruption point | Journal state observed | Invariant | Verifying test (T-10.2.1) |
|---|---|---|---|---|
| 1 | Before `Intent` fsync'd | No record | No sidecar, no staging file; the content is at its original path. | `writetrashinfo_sigkill_before_intent_leaves_no_trace` |
| 2 | After `Intent` fsync'd, before anything is written | `Intent`, no `Completion` | Same as #1. | `writetrashinfo_sigkill_after_intent_before_write_sidecar_absent` |
| 3 | Mid-write (staging sibling written, or written and fsync'd, not yet renamed) | `Intent`, no `Completion` | No file named `*.trashinfo` exists; a `.duet-partial-*` staging sibling does, holding a prefix of the planned content. It is the *backend's* staging name, which no journal record carries, so discard/resume find it by listing the directory for that exact shape. The content has not moved. | `writetrashinfo_sigkill_mid_write_sidecar_absent_and_staging_file_marked` |
| 4 | After the sidecar is published, before `Completion` fsync'd | `Intent`, no `Completion`, sidecar complete | Recovery reads the sidecar, finds exactly the planned content, and treats the step as done -- a redo would collide with it (`create_new`) and, being the gate of the content move, strand the file half-trashed. The gated `Rename` then runs. | `writetrashinfo_sigkill_after_write_before_completion_record_recovery_completes_the_trash` |
| 5 | After `Completion` fsync'd, content move not yet started | `Intent` + `Completion { Succeeded }`; nothing for the `Rename` | Sidecar durable, content still at its original path: the safe half of the ordering. The job is unfinished and a resume runs the move. | `writetrashinfo_sigkill_after_completion_record_content_not_yet_moved_is_resumable` |

---

## Table coverage summary

| Step kind | Interruption points enumerated |
|---|---|
| `CopyFile` | 7 |
| `Reflink` | 7 |
| `CreateDir` | 5 |
| `Rename` | 5 |
| `Link` | 5 |
| `SetMeta` | 5 |
| `Remove` | 6 (5 + the `Recursive`-only mid-removal split into 3a/3b) |
| `Verify` | 5 |
| `Symlink` | 4 |
| `WriteTrashInfo` | 5 |
| **Total rows** | **54** |

Every row names a distinct T-10.2.1 test. T-10.2.1's AC ("Every injection point satisfies the §9.3 invariant... 100%, no exceptions") is therefore a checklist of exactly these named tests — not an open-ended fuzz target. The first 45 are the rows T-2.3.2 wrote; `Symlink` and `WriteTrashInfo` are the two `Step` variants added since, and their rows were added with the suite. Any further `Step` variant must extend this table: `checklist.rs` fails the build of the suite when a row has no test or a test no row, and a new variant without rows shows up as a non-exhaustive `match` in `tests/data_safety/model.rs`.

## Batching (2026-09-06): what changed in the mechanism and why every row above still holds

Measured on a real home filesystem (`docs/perf-baseline.md`), the original
one-`write`-one-`fsync`-per-record protocol cost ~4.5 ms per record and
four records per copied file, so a 10 000-file copy spent ~200 s in the
journal against ~0.5 s of actual copying. Three changes, each argued
against the rows above:

1. **Group commit** (`Journal::append_batch`, the executor's
   `JournalHandle`). Records queued while the writer thread is busy are
   written with one `write` and one `fsync` and answered together. A
   record's reply still means "durable now"; the *ordering* rows above
   (intent durable before the side effect, completion after) are
   unchanged because a step only proceeds when its own reply arrives.
   Crash artifact: a torn *tail* of the last batch. This section
   originally claimed `SIGKILL` cannot tear a batch (one `write`; the page
   cache outlives the process). **That was wrong**, and T-10.2.1's stress
   runs produced the counter-example: the kernel copies a large `write`
   into the page cache a page at a time, and a fatal signal arriving
   partway cuts it short, leaving a prefix of the batch that ends
   mid-record. Power loss does the same. Nothing in a torn batch was ever
   acknowledged (the reply is sent after the fsync), so no step acted on
   it. `JournalReader::scan` drops a trailing run of unparseable lines
   and rejects an unparseable line followed by a valid one, which an
   append-only file cannot produce by crashing -- *provided nothing is
   appended after a torn tail*. Closing an interrupted job out
   (`Journal::resolve`) does append, and used to glue its first record
   onto the fragment, producing exactly the rejected shape and failing
   the whole startup scan. `Journal::open` now cuts a torn tail off (and
   restores a lost final newline) before any append.

2. **Intent windows** (`INTENT_WINDOW = 64`) for *redo-safe* steps only
   (`CreateDir`, `CopyFile`, `Reflink`, `SetMeta`, `Verify`): up to 64
   consecutive such steps have their `Intent`s journaled in one awaited
   batch before any of them runs. Every step that starts still has a
   durable intent (the row invariant). The new artifact is an intent for a
   step that *never started* before the crash; recovery lists it as
   incomplete and redoes it, and for these kinds a redo is a no-op or a
   conflict-policy decision (`CreateDir` on an existing directory,
   `SetMeta`/`Verify` applied again, a copy whose destination already
   exists). The partial-file name is chosen when the intent is journaled
   and handed to the step, so `orphaned_partials` stays exact.

3. **Fire-and-forget `Completion`** for the same redo-safe kinds. The
   record is queued in order and becomes durable with the next batch --
   at the latest with the awaited `JobFinished`. A crash in between costs
   a redundant redo of a finished step, never data: the `CopyFile` row's
   "destination renamed into place, completion not yet durable" case is
   now merely more likely, and its outcome (recovery re-plans the step,
   finds the destination present, applies the job's conflict policy --
   `Skip` by default) is unchanged.

Everything not redo-safe (`Rename`, `Remove`, `Link`, `Symlink`,
`WriteTrashInfo`) keeps the strict per-record awaited protocol: a redo of
a finished rename or delete fails on a missing source and would surface
a phantom error at recovery time, and deletes are journaled before
execution so the undo stack can trust the record. A batch write that
fails poisons the writer -- every later record, awaited or not, fails
with that error -- and `execute` reports it as a job error when the
awaited `JobFinished` append fails.

## How the suite verifies this table (T-10.2.1)

`crates/duet-ops/tests/data_safety/` is the release gate NFR-08 names.
Run it with `cargo nextest run -p duet-ops --test data_safety`; it is
part of the ordinary workspace test run. Its assertion is design.md
§14.4's sentence, checked after every fault it injects: *either the
source is intact and the destination is absent/partial-and-marked, or
the destination is complete. Never anything else.*

**How a kill lands on a row.** The executor and the local backend carry
named crash points (`duet_vfs::failpoint`). They exist only when the
crates are built with the `failpoints` feature, which only `duet-ops`'s
own test builds enable; in every other build they are empty inline
functions. A test re-executes the test binary as a child with
`DUET_FAILPOINT=<point>@<step>`; on reaching the point the child sends
itself a real `SIGKILL`. Nothing is simulated: no destructor runs, and
what the parent inspects is what the kernel kept.

| Row wording | Crash point |
|---|---|
| Before `Intent` fsync'd | `journal.intent.before_write@<step>` |
| After `Intent`, before the step acts | `step.before_effect@<step>` |
| Mid-write | `copy.mid_write@<step>`, `trashinfo.before_commit`, `vfs.write.before_publish` |
| After the write, before the publishing rename | `copy.before_publish@<step>` |
| Mid-apply (`SetMeta`) | `vfs.set_meta.phase_done@<phase>` |
| Mid-recursive-removal | `vfs.remove_recursive.unlinked#<n>` |
| Mid-hash (`Verify`) | `verify.mid_hash` |
| After the effect, before `Completion` | `step.after_effect@<step>` |
| After `Completion` fsync'd | `journal.completion.after_fsync@<step>` |

**The "mid-`<syscall>`" rows** describe a state user space cannot reach.
Their tests kill a child at an arbitrary instant while it runs 3000
steps of that kind, five times at different depths into the plan, and
require the invariant wherever the kill landed. The assertion never
depends on where that was, so the tests are deterministic; they fail if
no kill caught the job mid-plan.

**What "satisfies the invariant" means in the tests.** An independent
model (`model.rs`) predicts from the plan alone what each step does to
the tree. After a crash, every path no in-flight step touches must be
exactly what the journal's durably completed steps produce; every path
an in-flight step touches must be in its before- or after-state (or the
intermediate states this table allows for `SetMeta` and recursive
`Remove`); anything else on disk must be a `.duet-partial-*` file of an
in-flight step holding a prefix of its source; and no file content that
existed before the job may be gone. Then the crash is recovered the way
the product recovers it -- once by **resume**, which must produce
exactly the finished job with no scratch file, no phantom error and no
phantom skip, and (from a second, identical crash) once by **discard**,
which must leave a legal state with no scratch files.

**Beyond the table.** The same checks run after a kill at each of four
boundaries of *every* step of real `plan_copy` / `plan_move` /
`plan_delete` plans (`sigkill_sweep.rs`), after a second kill during
the recovery, after an I/O error or a disconnect at every backend call
(`faults.rs`), under real `EACCES` (`permissions.rs`) and real `ENOSPC`
(`enospc.rs`), and -- as root, in CI -- on a loop-device filesystem and
under `dm-flakey` (`privileged.rs`).

### What the suite found

A suite written to confirm a design tends to find where the
implementation left it. These were all present on `main` when the suite
was first run, and each is now pinned by the test named:

| # | Defect | Consequence | Pinned by |
|---|---|---|---|
| 1 | A step gated on another ran unless that one had *failed* -- a *skipped* prerequisite let it through. | **Data loss, no crash needed.** (a) A cross-device move whose copy was skipped on a name conflict deleted the source anyway, and stamped the source's mode and mtime onto the unrelated file already there. (b) A cross-device move *with verification on* whose copy failed: the verify was skipped, so the removal gated on the verify ran and deleted the uncopied source. | `conflicts::*`, `permissions::cross_device_move_with_an_unreadable_source_file_keeps_that_file` |
| 2 | A resume cleared a dependency on any step that had a `Completion`, including a failed one. | **Data loss.** Copy fails, the process dies before the gated `Remove` is recorded as skipped, the resume deletes the uncopied source. | `permissions::a_crash_after_a_failed_copy_...`, `verify_sigkill_after_completion_record_..._blocks_source_unlink` |
| 3 | The accelerated copy path never `fsync`'d; a cross-device move unlinked sources whose copies were only in the page cache. | **Data loss on power cut.** | `privileged::dm_flakey_power_loss_mid_move_...` |
| 4 | A resume redid only the steps with a dangling intent; a job with none dangling was not offered for recovery at all. | A job killed early "resumed" a few dozen files and reported success. | every `finish_by_resume` (`check_finished`) |
| 5 | A resume redid steps that had already taken effect. | Phantom "not found" errors for finished renames and deletes; a finished trash sidecar blocked the move gated on it. | the "recovery infers success" rows |
| 6 | A resume never removed the interrupted attempt's partials. | A full-size `.duet-partial-*` left for good after every resumed copy. | every `finish_by_resume` |
| 7 | Closing out a journal with a torn tail appended onto the fragment. | The journal became "corrupt", and one corrupt journal fails the whole startup scan -- for every job, at every start. | `a_journal_torn_mid_record_by_the_kill_...`, `journal::tests::appending_after_a_torn_tail_...` |
| 8 | A retry forgot the staging file its failed attempt could not remove. | A stray partial after a job that reported a clean finish. | `faults::*_rides_out_a_transient_disconnect_*` |
| 9 | A failed publish (the rename) left the fully staged copy behind. | A full-size scratch file nothing names. | `faults::*_survives_an_io_error_*` |
| 10 | Discard knew only the executor's partial names, not the backend's own staging siblings. | A killed trash-sidecar write or buffered copy left a file no recovery could find. | `writetrashinfo_sigkill_mid_write_...`, `copyfile_sigkill_mid_write_...` |
| 11 | "Re-run failed" did not re-apply directory metadata. | Destination directories stamped "now" instead of with the source's times. | `faults::*_survives_an_io_error_*` |
| 12 | A failed commit in the buffered copy kept its progress credit. | A retried file counted twice in the job's byte total. | (fixed alongside 9) |
| 13 | Steps that had *failed* before a crash counted as finished. | A job that hit an error and was then killed was not offered for recovery, or resumed to "completed" over files never copied; the error reached nobody. | `permissions::a_crash_after_a_reported_failure_does_not_bury_the_failure` |

### What recovery does with a row's journal state

The rows say what recovery *must* infer; `duet_ops::prepare_resume` is
where it now does:

- **"Recovery infers success" / "treats as done"** (the after-effect
  rows): the step's effect is looked for on disk -- the copy by content,
  the rename by its entry being at the destination and not the source,
  the link by inode, the symlink and the trash sidecar by exact content,
  the removal by absence -- and a step found done is left out of the
  resumed plan. `CreateDir`, `SetMeta` and `Verify` are simply redone.
- **The remaining work is not just the dangling intents.** A job killed
  at file 300 of 10 000 has a window of dangling intents and thousands
  of steps with no record. Both are resumed -- and so are the steps
  that had failed or been skipped before the kill, whose report died
  with the process.
- **A dependency with a `Completion` is not a dependency that
  succeeded.** A step gated on one the journal records as `Failed` or
  `Skipped` drags that step back into the resumed plan, so the gate is
  re-evaluated instead of cleared. (And inside a run, a gated step
  proceeds only if its prerequisite `Succeeded` -- not merely "did not
  fail". A step held back because its prerequisite was *skipped* is
  journaled as skipped but not listed in the job's report: one skipped
  file is one skip.)
- **Orphaned partials are removed by a resume**, not only by a discard:
  the resumed copy stages through a fresh name.

### Power loss is a different threat, with a narrower guarantee

Everything above is about `SIGKILL`: the process dies, the page cache
does not. Power loss also discards what was never flushed. Two things
hold there, and one does not:

- **The journal** is fsync'd before any step acts on it, and its reader
  tolerates a torn tail (see "Batching" above).
- **A cross-device move never unlinks a source before its copy is
  durable.** Every copy in a plan that removes sources on the strength
  of copies is `fsync`'d before it is published, and its directory
  `fsync`'d after (`FileSystem::sync`). The accelerated copy path used
  to leave the destination in the page cache, so a move could delete
  the source of a file that existed nowhere on stable storage.
- **A plain copy is not fsync'd.** After a power cut seconds after a
  copy "completed", a destination file can exist under its final name
  with less than its full content, with the journal saying the step
  succeeded -- the same exposure as `cp`. The source is intact, so
  nothing is lost, but "never a silently truncated destination" does
  not hold for power loss. Closing it costs one `fsync` per copied
  file, which on a rotating disk multiplies the time of a
  many-small-files copy; that trade is recorded as an open decision in
  `documentation/known_issues.md`, not taken silently in either
  direction.
