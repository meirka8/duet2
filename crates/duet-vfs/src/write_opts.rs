//! `WriteOpts`/`Mode` — options for
//! [`FileSystem::open_write`](crate::FileSystem::open_write) and
//! [`FileSystem::create_dir`](crate::FileSystem::create_dir).

/// POSIX mode bits (permission bits; file-type bits are meaningless here
/// since the caller already knows what kind of node it's creating). Wrapped
/// rather than a bare `u32` so call sites read `Mode::new(0o644)` instead of
/// an unlabelled magic number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Mode(u32);

impl Mode {
    /// The conventional default for a newly created regular file
    /// (`0o644`), before umask is applied by backends that honour umask.
    pub const DEFAULT_FILE: Mode = Mode(0o644);
    /// The conventional default for a newly created directory (`0o755`).
    pub const DEFAULT_DIR: Mode = Mode(0o755);

    pub const fn new(bits: u32) -> Self {
        Mode(bits)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// Options controlling
/// [`FileSystem::open_write`](crate::FileSystem::open_write).
///
/// There is deliberately no "overwrite policy" field here (skip/rename/
/// ask-the-user, etc.) — that belongs to `duet-ops`'s `ConflictPolicy`
/// (T-2.3.1), one layer up. `WriteOpts` only expresses what the *VFS
/// primitive itself* needs to know to open the destination correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WriteOpts {
    /// Create the destination if it does not already exist. If `false` and
    /// the destination is missing, `open_write` returns
    /// `Err(ErrorKind::NotFound)`.
    pub create: bool,
    /// If the destination exists and this is `true`, `open_write` fails
    /// with `Err(ErrorKind::Conflict)` instead of opening it (`O_EXCL`
    /// semantics) — the exclusive-create case a `Rename`-based conflict
    /// policy relies on to detect a race.
    pub fail_if_exists: bool,
    /// Truncate an existing destination to zero length before writing.
    /// Ignored when writing goes through a temp-sibling-then-`commit`
    /// implementation (the temp file starts empty regardless); meaningful
    /// for backends that write in place (see
    /// [`AsyncWriteCommit`](crate::AsyncWriteCommit)).
    pub truncate: bool,
    /// Append-only writes, positioned at the current end of the
    /// destination. Used for `Caps::APPEND_RESUME` (design.md §9.3):
    /// resuming an interrupted transfer without restarting it.
    pub append: bool,
    /// Permission bits for a newly created destination. `None` means "use
    /// the backend's default" ([`Mode::DEFAULT_FILE`], modulated by umask
    /// where that concept applies).
    pub mode: Option<Mode>,
    /// The final size, if known in advance. Not enforced locally, but some
    /// remote backends (S3-style PUT, SFTP with pre-allocation) can use it
    /// to avoid buffering the whole body or to pre-allocate; a `None` here
    /// just means the backend falls back to streaming/chunked writes.
    pub expected_size: Option<u64>,
}

impl WriteOpts {
    /// Create-or-overwrite a destination (the common copy/save case):
    /// creates if missing, truncates if present.
    pub fn overwrite() -> Self {
        WriteOpts {
            create: true,
            fail_if_exists: false,
            truncate: true,
            append: false,
            mode: None,
            expected_size: None,
        }
    }

    /// Exclusive create: fails with `Conflict` if the destination already
    /// exists. What a "never clobber" copy policy uses.
    pub fn create_new() -> Self {
        WriteOpts {
            create: true,
            fail_if_exists: true,
            truncate: false,
            append: false,
            mode: None,
            expected_size: None,
        }
    }

    /// Append to an existing destination (or create it if missing), for
    /// `APPEND_RESUME`-capable backends resuming an interrupted transfer.
    pub fn append_to() -> Self {
        WriteOpts {
            create: true,
            fail_if_exists: false,
            truncate: false,
            append: true,
            mode: None,
            expected_size: None,
        }
    }

    pub fn with_mode(mut self, mode: Mode) -> Self {
        self.mode = Some(mode);
        self
    }

    pub fn with_expected_size(mut self, size: u64) -> Self {
        self.expected_size = Some(size);
        self
    }
}

impl Default for WriteOpts {
    fn default() -> Self {
        WriteOpts::overwrite()
    }
}
