// SPDX-License-Identifier: MIT
//! The incremental line index (T-5.3.6, design.md §9.6): built in the
//! background after a file opens, consulted only for the status line's
//! "line N of M" and for `Ctrl+G <line>`. Scrolling never waits for it
//! (rows come from `text::Pager`), which is what keeps a 5 GB log inside
//! the 300 ms budget.
//!
//! Sparse: one checkpoint `(offset, line)` every [`CHECKPOINT_LINES`]
//! lines *or* [`CHECKPOINT_BYTES`] bytes, whichever comes first, so a
//! lookup reads at most one checkpoint's span -- bounded in bytes even
//! for the 200 MB single-line file, and ~50k entries for a 5 GB log
//! instead of 400 MB of per-line offsets.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::source::{ByteSource, CHUNK};
use super::text::{TextEncoding, newline_ends};

pub(crate) const CHECKPOINT_LINES: u64 = 1024;
pub(crate) const CHECKPOINT_BYTES: u64 = CHUNK;

#[derive(Debug)]
pub(crate) struct LineIndex {
    /// `(offset of a line start, its 0-based line number)`, ascending;
    /// the first is `(bom, 0)`.
    checkpoints: Vec<(u64, u64)>,
    /// Bytes scanned so far (from the BOM); newline counts are exact up
    /// to here.
    pub(crate) indexed_upto: u64,
    /// Newlines seen so far.
    pub(crate) newlines: u64,
    pub(crate) complete: bool,
    /// Whether the last byte scanned so far is a newline.
    ends_with_newline: bool,
    len: u64,
    bom: u64,
}

impl LineIndex {
    pub(crate) fn new(bom: u64, len: u64) -> Self {
        Self {
            checkpoints: vec![(bom, 0)],
            indexed_upto: bom.min(len),
            newlines: 0,
            complete: bom >= len,
            ends_with_newline: false,
            len,
            bom,
        }
    }

    /// 0.0 ..= 1.0.
    pub(crate) fn progress(&self) -> f32 {
        if self.len <= self.bom {
            return 1.0;
        }
        (self.indexed_upto - self.bom) as f32 / (self.len - self.bom) as f32
    }

    /// The number of lines once the scan is complete: newlines, plus
    /// the unterminated last line if there is one.
    pub(crate) fn total_lines(&self) -> Option<u64> {
        if !self.complete {
            return None;
        }
        if self.len <= self.bom {
            return Some(0);
        }
        Some(self.newlines + u64::from(!self.ends_with_newline))
    }

    /// The last checkpoint at or before `offset`, if `offset` has been
    /// scanned.
    pub(crate) fn checkpoint_before(&self, offset: u64) -> Option<(u64, u64)> {
        if offset > self.indexed_upto {
            return None;
        }
        let ix = self.checkpoints.partition_point(|(o, _)| *o <= offset);
        ix.checked_sub(1).map(|i| self.checkpoints[i])
    }

    /// The last checkpoint at or before 0-based `line`, if the scan has
    /// reached that line.
    pub(crate) fn checkpoint_for_line(&self, line: u64) -> Option<(u64, u64)> {
        if line > self.newlines {
            return None;
        }
        let ix = self.checkpoints.partition_point(|(_, l)| *l <= line);
        ix.checked_sub(1).map(|i| self.checkpoints[i])
    }
}

/// Scans `src` from the BOM to the end in [`CHUNK`] steps, publishing
/// into `index` after every chunk; `on_progress` runs after each
/// publish (the worker forwards it to the UI). Stops early when
/// `cancel` is set (the viewer closed or moved to another file).
pub(crate) fn build<S: ByteSource>(
    src: &mut S,
    enc: TextEncoding,
    index: &Mutex<LineIndex>,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(),
) -> std::io::Result<()> {
    let (bom, len) = {
        let ix = index.lock().unwrap_or_else(|e| e.into_inner());
        (ix.bom, ix.len)
    };
    let mut pos = bom;
    let mut newlines = 0u64;
    let mut last_cp = (bom, 0u64);
    let mut pending: Vec<(u64, u64)> = Vec::new();
    while pos < len {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let chunk = src.read(pos, CHUNK as usize)?;
        if chunk.is_empty() {
            break;
        }
        for end in newline_ends(enc, &chunk, pos, bom) {
            newlines += 1;
            let line_start = pos + end as u64;
            if newlines - last_cp.1 >= CHECKPOINT_LINES
                || line_start - last_cp.0 >= CHECKPOINT_BYTES
            {
                last_cp = (line_start, newlines);
                pending.push(last_cp);
            }
        }
        pos += chunk.len() as u64;
        let ends_with_newline = newline_ends(enc, &chunk, pos - chunk.len() as u64, bom)
            .last()
            .is_some_and(|e| e == chunk.len());
        {
            let mut ix = index.lock().unwrap_or_else(|e| e.into_inner());
            ix.checkpoints.append(&mut pending);
            ix.indexed_upto = pos;
            ix.newlines = newlines;
            ix.ends_with_newline = ends_with_newline;
            ix.complete = pos >= len;
        }
        on_progress();
    }
    let mut ix = index.lock().unwrap_or_else(|e| e.into_inner());
    ix.complete = true;
    Ok(())
}

/// 0-based line number of the line containing `offset`, if indexed.
pub(crate) fn line_of_offset<S: ByteSource>(
    src: &mut S,
    enc: TextEncoding,
    index: &Mutex<LineIndex>,
    offset: u64,
) -> std::io::Result<Option<u64>> {
    let (cp, bom) = {
        let ix = index.lock().unwrap_or_else(|e| e.into_inner());
        (ix.checkpoint_before(offset), ix.bom)
    };
    let Some((cp_off, cp_line)) = cp else {
        return Ok(None);
    };
    let mut line = cp_line;
    let mut pos = cp_off;
    while pos < offset {
        let want = ((offset - pos).min(CHUNK)) as usize;
        let chunk = src.read(pos, want)?;
        if chunk.is_empty() {
            break;
        }
        line += newline_ends(enc, &chunk, pos, bom).count() as u64;
        pos += chunk.len() as u64;
    }
    Ok(Some(line))
}

/// Start offset of 0-based `line`, if the scan has reached it; `None`
/// also for a line past the end of a complete index.
pub(crate) fn offset_of_line<S: ByteSource>(
    src: &mut S,
    enc: TextEncoding,
    index: &Mutex<LineIndex>,
    line: u64,
) -> std::io::Result<Option<u64>> {
    let (cp, bom, len) = {
        let ix = index.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(total) = ix.total_lines()
            && line >= total
        {
            return Ok(None);
        }
        (ix.checkpoint_for_line(line), ix.bom, ix.len)
    };
    let Some((cp_off, cp_line)) = cp else {
        return Ok(None);
    };
    let mut remaining = line - cp_line;
    let mut pos = cp_off;
    while remaining > 0 && pos < len {
        let chunk = src.read(pos, CHUNK as usize)?;
        if chunk.is_empty() {
            return Ok(None);
        }
        for end in newline_ends(enc, &chunk, pos, bom) {
            remaining -= 1;
            if remaining == 0 {
                return Ok(Some(pos + end as u64));
            }
        }
        pos += chunk.len() as u64;
    }
    Ok((remaining == 0 && pos < len).then_some(pos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::UTF_8;

    fn build_all(src: &mut Vec<u8>) -> Mutex<LineIndex> {
        let index = Mutex::new(LineIndex::new(0, src.len() as u64));
        let cancel = AtomicBool::new(false);
        let mut ticks = 0;
        build(src, TextEncoding(UTF_8), &index, &cancel, || ticks += 1).unwrap();
        assert!(ticks >= 1 || src.is_empty());
        index
    }

    #[test]
    fn counts_lines_and_resolves_both_ways() {
        let mut src = (0..5000)
            .map(|i| format!("line {i}\n"))
            .collect::<String>()
            .into_bytes();
        let index = build_all(&mut src);
        {
            let ix = index.lock().unwrap();
            assert!(ix.complete);
            assert_eq!(ix.total_lines(), Some(5000));
            assert!(ix.checkpoints.len() >= 5, "a checkpoint every 1024 lines");
        }
        let enc = TextEncoding(UTF_8);
        let off_2500 = offset_of_line(&mut src, enc, &index, 2500)
            .unwrap()
            .unwrap();
        assert!(src[off_2500 as usize..].starts_with(b"line 2500\n"));
        assert_eq!(
            line_of_offset(&mut src, enc, &index, off_2500).unwrap(),
            Some(2500)
        );
        assert_eq!(
            line_of_offset(&mut src, enc, &index, off_2500 + 3).unwrap(),
            Some(2500)
        );
        assert_eq!(offset_of_line(&mut src, enc, &index, 0).unwrap(), Some(0));
        assert_eq!(offset_of_line(&mut src, enc, &index, 5000).unwrap(), None);
        assert_eq!(line_of_offset(&mut src, enc, &index, 0).unwrap(), Some(0));
    }

    #[test]
    fn unterminated_last_line_counts_and_byte_checkpoints_bound_long_lines() {
        let mut src = b"a\nb\nc".to_vec();
        let index = build_all(&mut src);
        assert_eq!(index.lock().unwrap().total_lines(), Some(3));
        assert_eq!(
            offset_of_line(&mut src, TextEncoding(UTF_8), &index, 2).unwrap(),
            Some(4)
        );

        let mut empty = Vec::new();
        let index = build_all(&mut empty);
        assert_eq!(index.lock().unwrap().total_lines(), Some(0));

        // 3 MiB with a newline only every 1.5 MiB: byte-driven checkpoints.
        let mut long = vec![b'x'; 3 * CHUNK as usize];
        long[CHUNK as usize + CHUNK as usize / 2] = b'\n';
        let index = build_all(&mut long);
        let ix = index.lock().unwrap();
        assert_eq!(ix.total_lines(), Some(2));
        assert!(ix.checkpoint_before(2 * CHUNK).is_some());
    }

    #[test]
    fn cancellation_stops_the_scan_and_lookups_stay_partial() {
        let mut src = vec![b'\n'; 3 * CHUNK as usize];
        let index = Mutex::new(LineIndex::new(0, src.len() as u64));
        let cancel = AtomicBool::new(false);
        build(&mut src, TextEncoding(UTF_8), &index, &cancel, || {
            cancel.store(true, Ordering::Relaxed)
        })
        .unwrap();
        let ix = index.lock().unwrap();
        assert!(!ix.complete);
        assert_eq!(ix.indexed_upto, CHUNK);
        assert!(ix.checkpoint_before(CHUNK + 1).is_none());
        assert!(ix.checkpoint_before(CHUNK).is_some());
    }
}
