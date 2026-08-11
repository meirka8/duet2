# Crash-safety proof sketch (T-2.3.2)

**Traces:** FR-OPS-07, NFR-08.
**Types referenced:** `crates/duet-ops/src/step.rs` (`Step`, `StepKind`), `crates/duet-ops/src/journal.rs` (`JournalRecord`, `StepOutcome`).

## Scope and method

FR-OPS-07 states the guarantee this document exists to justify:

> An interrupted operation leaves either the old file intact or a clearly-marked partial file, never a silently truncated destination. Journal permits resume.

For every [`Step`](../crates/duet-ops/src/step.rs) kind defined in T-2.3.1, this document enumerates every point in that step's execution where a `SIGKILL` (or an equivalent hard interruption — power loss, `OOM-killer`, `kill -9`) can land, and states the on-disk invariant that must hold immediately afterward. Each row names the test in the future data-safety suite (**T-10.2.1**, task.md: "SIGKILL injection at every step boundary... Every injection point satisfies the §9.3 invariant. Release gate: 100%, no exceptions, no 'flaky' annotations") that is responsible for proving that invariant by actually injecting a `SIGKILL` at that point and inspecting the result. T-10.2.1 does not exist yet; the names below are the contract it must fulfil, established now so there is no ambiguity later about what "every step × every interruption point" means.

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
| **Total rows** | **45** |

Every row names a distinct T-10.2.1 test. T-10.2.1's AC ("Every injection point satisfies the §9.3 invariant... 100%, no exceptions") is therefore, as of this document, a checklist of exactly 45 named tests — not an open-ended fuzz target. New `Step` variants added after T-2.3.1 (there are none currently planned) must extend this table before T-10.2.1 can claim its release-gate coverage is still complete.
