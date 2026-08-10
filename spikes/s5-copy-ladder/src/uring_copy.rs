//! `io_uring` batching strategy for the many-small-files corpus.
//!
//! Design: process files in batches of `batch` (default 256). For each
//! batch, issue four *phases*, each phase being one `io_uring_enter` call
//! covering the whole batch, rather than 6 syscalls per file done one at a
//! time (open src, open dst, read, write, close src, close dst):
//!
//!   1. open  — 2*n `OpenAt` SQEs (src RDONLY, dst WRONLY|CREAT|TRUNC), one submit+wait
//!   2. read  — n `Read` SQEs into per-slot buffers, one submit+wait
//!   3. write — n `Write` SQEs sized to the actual bytes each Read returned, one submit+wait
//!   4. close — 2*n `Close` SQEs, one submit+wait
//!
//! This is a batched-submission model, not chained/linked SQEs: Write's
//! length depends on Read's result, and io_uring has no built-in mechanism
//! to thread one op's result into the next op's length field within a
//! linked chain, so phases 2 and 3 are deliberately separate submissions.
//! A production implementation would likely add registered/fixed file
//! descriptors and provided buffers to cut overhead further; that's out of
//! scope for a timeboxed spike whose job is to answer "is the *batching*
//! win here real" (see S-5.md for the go/no-go call).

use anyhow::{bail, Context, Result};
use io_uring::{opcode, types, IoUring};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Upper bound on a single small file's size. The corpus files are 1-8 KiB
/// each; this just needs to be >= the largest file so a single Read call
/// captures the whole file (short reads are fine and expected). Kept close
/// to the actual corpus ceiling deliberately: an earlier version of this
/// spike used 1 MiB here, which meant every in-flight batch slot allocated
/// and zeroed a 1 MiB `Vec<u8>` for a file that was typically ~4 KiB — with
/// `batch` in-flight slots that's `batch` MiB of memset per batch for a few
/// hundred KiB of real payload, which dominated wall time and made
/// io_uring look far slower than a plain per-file loop. Buffer sizing, not
/// io_uring's batching itself, was the bottleneck. A production
/// implementation would size this from a stat() or from provided buffers
/// rather than a hardcoded constant.
const MAX_SMALL_FILE: usize = 64 * 1024; // 64 KiB (8x headroom over the 8 KiB corpus ceiling)

pub struct UringOutcome {
    pub files: usize,
    pub bytes: u64,
    pub duration: Duration,
}

fn cstring(p: &Path) -> Result<CString> {
    CString::new(p.as_os_str().as_bytes()).context("path contains NUL byte")
}

fn encode(stage: u8, idx: u32) -> u64 {
    ((stage as u64) << 32) | (idx as u64)
}
fn decode(v: u64) -> (u8, u32) {
    ((v >> 32) as u8, (v & 0xFFFF_FFFF) as u32)
}

/// Copy `pairs` (src, dst) using batched io_uring submissions. Returns an
/// error (rather than panicking) if `io_uring_setup` fails, e.g. because the
/// kernel doesn't support it or it's blocked by seccomp in this environment
/// — the caller is expected to treat that as a documented finding, not a
/// bug in the harness.
pub fn uring_batch_copy(
    pairs: &[(PathBuf, PathBuf)],
    batch: usize,
    queue_depth: u32,
) -> Result<UringOutcome> {
    let mut ring = IoUring::new(queue_depth)
        .context("IoUring::new failed (kernel/seccomp may not support io_uring)")?;
    let start = Instant::now();
    let mut total_bytes: u64 = 0;
    let mut total_files: usize = 0;

    for chunk in pairs.chunks(batch) {
        let n = chunk.len();
        let src_c: Vec<CString> = chunk
            .iter()
            .map(|(s, _)| cstring(s))
            .collect::<Result<_>>()?;
        let dst_c: Vec<CString> = chunk
            .iter()
            .map(|(_, d)| cstring(d))
            .collect::<Result<_>>()?;

        // --- Phase 1: open src + dst (2n SQEs, one submit) ---
        let mut src_fds = vec![-1i32; n];
        let mut dst_fds = vec![-1i32; n];
        unsafe {
            let mut sq = ring.submission();
            for i in 0..n {
                let open_src = opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), src_c[i].as_ptr())
                    .flags(libc::O_RDONLY)
                    .build()
                    .user_data(encode(0, i as u32));
                sq.push(&open_src)
                    .map_err(|_| anyhow::anyhow!("SQ full (open src)"))?;
                let open_dst = opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), dst_c[i].as_ptr())
                    .flags(libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC)
                    .mode(0o644)
                    .build()
                    .user_data(encode(1, i as u32));
                sq.push(&open_dst)
                    .map_err(|_| anyhow::anyhow!("SQ full (open dst)"))?;
            }
        }
        ring.submit_and_wait(2 * n)?;
        {
            let mut cq = ring.completion();
            cq.sync();
            let mut seen = 0usize;
            for cqe in &mut cq {
                seen += 1;
                let (stage, idx) = decode(cqe.user_data());
                let res = cqe.result();
                if res < 0 {
                    bail!("open (stage {stage}) failed for idx {idx}: errno {}", -res);
                }
                if stage == 0 {
                    src_fds[idx as usize] = res;
                } else {
                    dst_fds[idx as usize] = res;
                }
            }
            if seen != 2 * n {
                bail!("expected {} open completions, got {}", 2 * n, seen);
            }
        }

        // --- Phase 2: read (n SQEs, one submit) ---
        let mut bufs: Vec<Vec<u8>> = (0..n).map(|_| vec![0u8; MAX_SMALL_FILE]).collect();
        unsafe {
            let mut sq = ring.submission();
            for i in 0..n {
                let e = opcode::Read::new(
                    types::Fd(src_fds[i]),
                    bufs[i].as_mut_ptr(),
                    MAX_SMALL_FILE as u32,
                )
                .build()
                .user_data(encode(2, i as u32));
                sq.push(&e).map_err(|_| anyhow::anyhow!("SQ full (read)"))?;
            }
        }
        ring.submit_and_wait(n)?;
        let mut read_len = vec![0u32; n];
        {
            let mut cq = ring.completion();
            cq.sync();
            let mut seen = 0usize;
            for cqe in &mut cq {
                seen += 1;
                let (_stage, idx) = decode(cqe.user_data());
                let res = cqe.result();
                if res < 0 {
                    bail!("read failed for idx {idx}: errno {}", -res);
                }
                read_len[idx as usize] = res as u32;
            }
            if seen != n {
                bail!("expected {} read completions, got {}", n, seen);
            }
        }

        // --- Phase 3: write (n SQEs, one submit) ---
        unsafe {
            let mut sq = ring.submission();
            for i in 0..n {
                let e = opcode::Write::new(types::Fd(dst_fds[i]), bufs[i].as_ptr(), read_len[i])
                    .build()
                    .user_data(encode(3, i as u32));
                sq.push(&e)
                    .map_err(|_| anyhow::anyhow!("SQ full (write)"))?;
            }
        }
        ring.submit_and_wait(n)?;
        {
            let mut cq = ring.completion();
            cq.sync();
            let mut seen = 0usize;
            for cqe in &mut cq {
                seen += 1;
                let res = cqe.result();
                if res < 0 {
                    bail!("write failed: errno {}", -res);
                }
                total_bytes += res as u64;
            }
            if seen != n {
                bail!("expected {} write completions, got {}", n, seen);
            }
        }

        // --- Phase 4: close src + dst (2n SQEs, one submit) ---
        unsafe {
            let mut sq = ring.submission();
            for i in 0..n {
                let c1 = opcode::Close::new(types::Fd(src_fds[i]))
                    .build()
                    .user_data(encode(4, i as u32));
                sq.push(&c1)
                    .map_err(|_| anyhow::anyhow!("SQ full (close src)"))?;
                let c2 = opcode::Close::new(types::Fd(dst_fds[i]))
                    .build()
                    .user_data(encode(5, i as u32));
                sq.push(&c2)
                    .map_err(|_| anyhow::anyhow!("SQ full (close dst)"))?;
            }
        }
        ring.submit_and_wait(2 * n)?;
        {
            let mut cq = ring.completion();
            cq.sync();
            let mut seen = 0usize;
            for cqe in &mut cq {
                seen += 1;
                let res = cqe.result();
                if res < 0 {
                    bail!("close failed: errno {}", -res);
                }
            }
            if seen != 2 * n {
                bail!("expected {} close completions, got {}", 2 * n, seen);
            }
        }

        total_files += n;
    }

    Ok(UringOutcome {
        files: total_files,
        bytes: total_bytes,
        duration: start.elapsed(),
    })
}

/// Cheap probe: can we even create an io_uring instance here? Used to
/// produce a clean documented finding instead of a panic when io_uring is
/// unavailable (older kernel, or blocked by a container seccomp profile).
pub fn probe() -> Result<()> {
    let _ring = IoUring::new(8)?;
    Ok(())
}
