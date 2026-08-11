//! `AsyncReadSeek` and `AsyncWriteCommit` — the object-safe read/write
//! handles [`FileSystem::open_read`](crate::FileSystem::open_read) and
//! [`FileSystem::open_write`](crate::FileSystem::open_write) hand back.

use async_trait::async_trait;
use duet_types::Result;
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite};

/// A readable, seekable handle to an open source. Blanket-implemented for
/// any type that is already `AsyncRead + AsyncSeek + Send + Unpin` (e.g.
/// `tokio::fs::File`), so backends don't hand-write an impl of this trait —
/// they just return the underlying reader boxed as `Box<dyn
/// AsyncReadSeek>`.
///
/// `Seek` is part of the contract (not a separate capability) because the
/// viewer/quick-look and resumable-transfer paths both need to seek; a
/// backend that genuinely can't seek (a raw pipe/forward-only stream) is
/// expected to wrap itself in a buffering adapter that fakes bounded
/// seeking rather than not implementing this trait at all — `open_read`'s
/// return type has no "forward-only" escape hatch by design, to keep
/// callers from having to branch on it.
pub trait AsyncReadSeek: AsyncRead + AsyncSeek + Send + Unpin {}

impl<T: AsyncRead + AsyncSeek + Send + Unpin> AsyncReadSeek for T {}

/// Reports how durably a completed write landed, so the engine can warn the
/// user when a backend couldn't give the atomic-replace guarantee it would
/// prefer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CommitOutcome {
    /// `true` if the destination became visible via a single atomic
    /// operation (rename-over-destination, `Caps::ATOMIC_REPLACE`) — a
    /// reader can never observe a partially-written destination. `false`
    /// means the implementation wrote in place (e.g. plain FTP `STOR` has
    /// no atomic-replace primitive) and a crash mid-write can leave a
    /// truncated/corrupt destination; the engine surfaces this as a
    /// reduced-durability warning (design.md §9.1) rather than failing the
    /// operation outright.
    pub atomic: bool,
    /// Total bytes written before `commit` was called.
    pub bytes_written: u64,
}

/// A write handle that separates *writing* from *publishing*
/// (design.md §9.1).
///
/// The write side is an ordinary `AsyncWrite` — callers use
/// `tokio::io::AsyncWriteExt` on it like any other writer. The publishing
/// side is the trait's own two terminal, consuming-`self` methods:
///
/// - [`commit`](AsyncWriteCommit::commit) — make the write visible at the
///   destination path. Implementations that opened a temporary sibling
///   (`.duet-partial-<rand>`, design.md §9.3's journal contract) rename it
///   over the destination here, atomically where `Caps::ATOMIC_REPLACE`
///   allows (`renameat2`); implementations that had no choice but to write
///   in place (FTP) just report `atomic: false` in the returned
///   [`CommitOutcome`].
/// - [`abort`](AsyncWriteCommit::abort) — discard the write. Implementations
///   that used a temp sibling delete it; implementations that wrote in
///   place leave the (now known-partial) destination exactly as journaling
///   requires (design.md §9.3: "A SIGKILL therefore leaves the old
///   destination intact and a visible partial file") rather than trying to
///   restore a prior version themselves.
///
/// Both methods take `self: Box<Self>` specifically so the type system
/// enforces "call exactly one of commit/abort" at the API boundary — there
/// is no `close()`-and-forget path that would leave the destination's fate
/// ambiguous. A handle dropped without either call is *not* guaranteed to
/// commit or to clean up (implementations may `unlink` a stray temp file in
/// `Drop` as a best-effort safety net, but callers must not rely on it —
/// the journal, not `Drop`, is what makes an interrupted write recoverable
/// after a `SIGKILL`, which skips `Drop` entirely).
#[async_trait]
pub trait AsyncWriteCommit: AsyncWrite + Send + Unpin {
    /// Publishes the write to the destination path. Errors:
    /// - `ErrorKind::Space` — the final rename/flush hit `ENOSPC`/`EDQUOT`.
    /// - `ErrorKind::Permission` — lost write access between `open_write`
    ///   and `commit` (rare, but real on multi-actor filesystems).
    /// - `ErrorKind::Conflict` — the destination was replaced by something
    ///   incompatible (e.g. became a directory) since `open_write`.
    /// - `ErrorKind::Retryable` / `ErrorKind::Fatal` — as for any I/O.
    async fn commit(self: Box<Self>) -> Result<CommitOutcome>;

    /// Discards the write; the destination is left as if `open_write` had
    /// never been called (temp-sibling implementations) or as a marked
    /// partial file (in-place implementations — see the trait doc comment).
    /// Errors: `ErrorKind::Permission`/`ErrorKind::Retryable`/
    /// `ErrorKind::Fatal` if the cleanup itself (e.g. unlinking the temp
    /// sibling) fails; the write is still considered not-committed even if
    /// `abort` itself errors.
    async fn abort(self: Box<Self>) -> Result<()>;
}
