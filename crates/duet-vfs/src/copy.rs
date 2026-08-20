//! `CopyOutcome` — the result of
//! [`FileSystem::server_side_copy`](crate::FileSystem::server_side_copy).

/// Outcome of a backend-accelerated copy attempt.
///
/// `Unsupported` is a normal, expected `Ok` result — not an error — for any
/// pair of paths the backend can't accelerate (design.md §9.1: "returns
/// `Unsupported` to fall back"). The engine's copy-strategy ladder
/// (design.md §9.3: `FICLONE` → `copy_file_range` → buffered copy) treats
/// this as "proceed to the next rung," not as a failed job step. Genuine
/// failures (source vanished, permission denied, out of space on the
/// destination) are `Err(VfsError)` with the appropriate `ErrorKind`, same
/// as any other `FileSystem` method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    /// The backend performed the copy itself.
    Copied {
        /// Bytes copied, for progress accounting.
        bytes: u64,
        /// `true` if this was a copy-on-write reflink (`FICLONE`) rather
        /// than a real data copy — lets the engine attribute it correctly
        /// in throughput stats (a reflink shouldn't count toward "MB/s"
        /// the way a real copy does) and skip a would-be-redundant
        /// `Verify` step, since a reflink can't have corrupted data in
        /// transit.
        reflinked: bool,
    },
    /// This backend (or this particular `from`/`to` pair — e.g. different
    /// mounts) cannot accelerate the copy. The caller must fall back to
    /// `open_read` + `open_write` + the copy-strategy ladder.
    Unsupported,
    /// `on_progress` returned `true` before the copy finished. The
    /// destination is left exactly as a `SIGKILL` at that same point would
    /// have (per `docs/crash-safety.md`'s `CopyFile`/`Reflink` invariants,
    /// which this outcome deliberately mirrors rather than sidesteps): if
    /// `to` was ever created, it exists but holds fewer bytes than
    /// planned, or none — never the pre-existing content of a path that
    /// happened to already be there (`server_side_copy` never targets an
    /// already-existing `to` in the first place; see `Conflict` above).
    /// The caller (T-5.1.3's executor) treats this exactly like the
    /// pause/cancel interruption its own buffered fallback loop already
    /// produces: discard and retry the whole step from scratch.
    Interrupted,
}
