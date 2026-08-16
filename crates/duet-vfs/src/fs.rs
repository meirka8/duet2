//! The `FileSystem` trait (design.md §9.1) — the central abstraction every
//! Duet backend (local, archive, remote) implements, and the only thing
//! anything above `duet-vfs` addresses files through.

use async_trait::async_trait;
use duet_types::{Caps, MetaPatch, Metadata, Result, VPath};
use futures_core::stream::BoxStream;

use crate::{AsyncReadSeek, AsyncWriteCommit, ChangeEvent, CopyOutcome, DirEntry};
use crate::{ListOpts, Mode, RemoveKind, RenameFlags, VolumeStats, WriteOpts};

/// A mounted filesystem backend: local disk, an archive opened as a
/// directory, or a remote protocol (SFTP/FTP/WebDAV/S3/SMB).
///
/// Every method that touches the backing store is `async` (via
/// `#[async_trait]`, not native async-fn-in-trait — see the crate-level
/// object-safety note) or returns an already-boxed stream, which is what
/// keeps `dyn FileSystem` usable: the runtime mount table is `MountId ->
/// Arc<dyn FileSystem>` (design.md §9.1), so this trait *must* support
/// `Arc<dyn FileSystem>`/`Box<dyn FileSystem>` construction. Native
/// async-fn-in-trait desugars each `async fn` to a method returning `->
/// impl Future`, and `impl Trait` return types cannot be part of a trait
/// object's vtable (the concrete future type varies per implementor and
/// has no fixed size) — such a trait fails to be object-safe and `dyn
/// FileSystem` would not compile. `#[async_trait]` sidesteps this by
/// desugaring instead to `fn(...) -> Pin<Box<dyn Future<Output = ...> +
/// Send + '_>>`, a concrete, sized, object-safe return type, at the cost of
/// one heap allocation per call — an acceptable trade here since every
/// method already does I/O (µs-to-ms scale) that dwarfs a single `Box::new`
/// (ns scale).
///
/// # Error semantics, generally
///
/// Every fallible method returns `duet_types::Result<T>` = `Result<T,
/// VfsError>`, where `VfsError::kind()` is one of the six
/// [`duet_types::ErrorKind`] buckets. Each method's doc comment below lists
/// which buckets it can legitimately return and why; a `VfsError` should
/// always carry `.with_path(...)` set to the operand path so callers (in
/// particular the ops engine's conflict/retry UI) can report *which* path
/// failed without re-deriving it from context.
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// This backend's URI scheme (`"file"`, `"zip"`, `"sftp"`, ...),
    /// matching the scheme a mounted [`VPath`] under it would display.
    /// Infallible, never returns an error.
    fn scheme(&self) -> &'static str;

    /// The capabilities this backend advertises. Infallible: a backend that
    /// genuinely doesn't know some capability in advance (rare) should
    /// advertise the pessimistic answer (capability absent) rather than
    /// making this fallible — callers degrade strategy on missing caps
    /// rather than erroring (design.md §9.1: "capability-driven strategy,
    /// not capability-driven refusal").
    fn caps(&self) -> Caps;

    /// Streams a directory's contents in chunks, so the panel can render
    /// the first chunk before the last arrives (design.md §9.1).
    ///
    /// Returns a stream rather than a single `Result<Vec<DirEntry>>` so a
    /// hundred-thousand-entry directory doesn't force the caller to wait
    /// for the whole listing (or the backend to buffer it all) before
    /// anything is visible; each yielded `Result` is one chunk, and a chunk
    /// erroring does not necessarily mean prior chunks were invalid.
    ///
    /// # Errors (per yielded item)
    /// - `NotFound` — `p` does not exist, or vanished mid-listing.
    /// - `Permission` — `p` exists but isn't readable/searchable.
    /// - `Conflict` — `p` exists but is not a directory (`ENOTDIR`).
    /// - `Retryable` — transient I/O (network blip on a remote backend).
    /// - `Fatal` — anything else (e.g. a `d_type`/protocol response the
    ///   backend can't parse).
    ///
    /// A terminal error ends the stream; it does not resume on its own.
    fn read_dir(&self, p: &VPath, opts: ListOpts) -> BoxStream<'_, Result<Vec<DirEntry>>>;

    /// Fetches metadata for a single path.
    ///
    /// `follow`: if `true`, and `p` is a symlink, returns metadata for the
    /// link's *target* (`stat` semantics); if `false`, returns metadata for
    /// the link itself (`lstat` semantics).
    ///
    /// # Errors
    /// - `NotFound` — `p` does not exist (or, with `follow: true`, its
    ///   target doesn't).
    /// - `Permission` — a directory component of `p` is not searchable.
    /// - `Fatal` — a symlink loop (`ELOOP`) with `follow: true`, or any
    ///   other unclassified failure.
    /// - `Retryable` — transient I/O.
    async fn stat(&self, p: &VPath, follow: bool) -> Result<Metadata>;

    /// Free/total space for the volume containing `p` (T-4.2.7's per-panel
    /// free-space indicator).
    ///
    /// # Errors
    /// - `NotFound` — `p` does not exist.
    /// - `Permission` — a directory component of `p` is not searchable.
    /// - `Fatal` — the backend has no meaningful concept of "volume" (an
    ///   archive or a backend where the question doesn't apply), or any
    ///   other unclassified failure.
    /// - `Retryable` — transient I/O.
    async fn volume_stats(&self, p: &VPath) -> Result<VolumeStats>;

    /// Opens `p` for reading, returning a seekable async reader.
    ///
    /// # Errors
    /// - `NotFound` — `p` does not exist.
    /// - `Permission` — `p` exists but is not readable.
    /// - `Conflict` — `p` is a directory (`EISDIR`); use `read_dir`
    ///   instead.
    /// - `Retryable` / `Fatal` — as for any I/O.
    async fn open_read(&self, p: &VPath) -> Result<Box<dyn AsyncReadSeek>>;

    /// Opens `p` for writing per `o`, returning a handle that separates
    /// writing from publishing (see [`AsyncWriteCommit`]'s doc comment for
    /// the full write-then-commit contract).
    ///
    /// # Errors
    /// - `NotFound` — `o.create` is `false` and `p` does not exist, or `p`'s
    ///   parent directory does not exist.
    /// - `Conflict` — `o.fail_if_exists` is `true` and `p` already exists.
    /// - `Permission` — `p` (or its parent, for creation) is not writable.
    /// - `Space` — the backend can detect up front that there isn't room
    ///   (some backends only discover this later, on `commit` or even on
    ///   individual writes — see [`AsyncWriteCommit::commit`]).
    /// - `Retryable` / `Fatal` — as for any I/O.
    async fn open_write(&self, p: &VPath, o: WriteOpts) -> Result<Box<dyn AsyncWriteCommit>>;

    /// Creates a directory at `p`. Non-recursive: the parent must already
    /// exist (mirrors `mkdirat`, not `mkdir -p`; recursive creation is a
    /// caller-side loop over path components, since only the caller knows
    /// whether creating missing ancestors is desired or a bug).
    ///
    /// # Errors
    /// - `Conflict` — `p` already exists (`EEXIST`).
    /// - `NotFound` — `p`'s parent does not exist.
    /// - `Permission` — parent is not writable.
    /// - `Space` — `ENOSPC`/`EDQUOT`.
    /// - `Fatal` — the backend doesn't support directories at all (rare;
    ///   e.g. some flat object-store backends), or any other failure.
    async fn create_dir(&self, p: &VPath, mode: Option<Mode>) -> Result<()>;

    /// Removes the entry at `p`, per `kind` — see [`RemoveKind`] for the
    /// file/empty-dir/recursive distinction and its own error notes.
    ///
    /// # Errors
    /// - `NotFound` — `p` does not exist.
    /// - `Permission` — `p` (or, for `Recursive`, some descendant) is not
    ///   writable/removable.
    /// - `Conflict` — kind/target mismatch (`RemoveKind::File` on a
    ///   directory, `RemoveKind::EmptyDir` on a non-empty one).
    /// - `Retryable` / `Fatal` — as for any I/O.
    async fn remove(&self, p: &VPath, kind: RemoveKind) -> Result<()>;

    /// Renames/moves `from` to `to` within this backend, per `flags` — see
    /// [`RenameFlags`].
    ///
    /// This is **not** expected to work across mounts/backends (a caller
    /// asking to move `file:///a` to `sftp://host/b` must go through
    /// copy-then-remove at the ops-engine layer, not through this method);
    /// within a single local backend it is also not guaranteed to work
    /// across filesystems (`EXDEV`) — design.md §9.3's move algorithm
    /// handles that by falling back to copy+verify+unlink at the engine
    /// layer when this method fails, so implementations should let `EXDEV`
    /// surface rather than silently emulating cross-device rename
    /// themselves.
    ///
    /// # Errors
    /// - `NotFound` — `from` does not exist.
    /// - `Conflict` — `to` exists and `RenameFlags::NO_REPLACE` was set, or
    ///   `to` is a non-empty directory and `from` isn't, or similar
    ///   structural conflicts.
    /// - `Permission` — either path's parent is not writable.
    /// - `Fatal` — cross-device rename (`EXDEV`; see above — the caller is
    ///   expected to retry via copy+remove, this is not this method's job),
    ///   `RENAME_EXCHANGE` requested on a backend that can't support atomic
    ///   exchange, or any other unclassified failure.
    /// - `Retryable` — transient I/O.
    async fn rename(&self, from: &VPath, to: &VPath, flags: RenameFlags) -> Result<()>;

    /// Applies a metadata patch to `p`. Only fields set in `m` are changed
    /// (see [`MetaPatch`]'s own doc comment); an empty patch
    /// (`MetaPatch::is_empty()`) is a valid no-op call, not an error.
    ///
    /// Implementations should apply fields in the order design.md §9.3
    /// specifies for the ops engine's own metadata-copy step (mode, then
    /// xattrs/ACL/SELinux label, then timestamps last, since writing
    /// xattrs perturbs ctime) whenever `m` sets more than one category, so
    /// backend behavior matches what the engine assumes when it calls this
    /// as one of several `SetMeta` steps.
    ///
    /// # Errors
    /// - `NotFound` — `p` does not exist.
    /// - `Permission` — `p` is not writable, or the caller lacks privilege
    ///   for the specific field being changed (e.g. `chown` without
    ///   `CAP_CHOWN`).
    /// - `Fatal` — the backend doesn't support a field the patch sets
    ///   (e.g. xattrs on a backend with no xattr concept); there is no
    ///   `ErrorKind::Unsupported`, so this is the correct bucket — callers
    ///   that want capability-aware behavior should consult `caps()`
    ///   before calling rather than relying on this error.
    /// - `Retryable` — transient I/O.
    async fn set_meta(&self, p: &VPath, m: &MetaPatch) -> Result<()>;

    /// Subscribes to change events under `p`. Only meaningful when
    /// `caps()` includes `Caps::WATCH`; a backend without push-based
    /// watching should return `Err(ErrorKind::Fatal)` immediately rather
    /// than returning a stream that never yields, so callers can tell
    /// "unsupported" apart from "supported but currently quiet" —
    /// `duet-index`'s polling fallback (design.md §9.2) is what callers
    /// should use instead when this errors.
    ///
    /// # Errors
    /// - `NotFound` — `p` does not exist at subscribe time.
    /// - `Permission` — `p` is not readable/searchable.
    /// - `Fatal` — `Caps::WATCH` absent, or the underlying watch mechanism
    ///   failed to initialize (e.g. inotify instance/watch limits
    ///   exhausted).
    ///
    /// Once subscribed, a terminal item is always
    /// [`ChangeEvent::Overflow`] or the stream simply ending (backend
    /// dropped the subscription, e.g. the watched mount was unmounted) —
    /// there is no separate error type yielded mid-stream, by design: a
    /// watch that can fail per-item would need a resync protocol this
    /// trait doesn't define.
    fn watch(&self, p: &VPath) -> Result<BoxStream<'_, ChangeEvent>>;

    /// Attempts a backend-accelerated same-filesystem copy (reflink,
    /// server-side copy API, etc.). Returns `Ok(CopyOutcome::Unsupported)`
    /// — not an error — when this backend/path pair can't accelerate the
    /// copy; see [`CopyOutcome`]'s doc comment.
    ///
    /// # Errors
    /// Only for genuine failures, never for "can't accelerate":
    /// - `NotFound` — `from` does not exist.
    /// - `Conflict` — `to` already exists (this method does not implement
    ///   any overwrite policy; callers that want overwrite semantics
    ///   should `remove`/`open_write` instead, or pre-check).
    /// - `Permission` — `from` not readable or `to`'s parent not writable.
    /// - `Space` — accelerated copy still consumes destination space
    ///   (except a true reflink, which the backend should attempt first
    ///   internally when available) and can hit `ENOSPC`/`EDQUOT`.
    /// - `Retryable` / `Fatal` — as for any I/O.
    async fn server_side_copy(&self, from: &VPath, to: &VPath) -> Result<CopyOutcome>;
}
