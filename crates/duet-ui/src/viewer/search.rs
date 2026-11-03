// SPDX-License-Identifier: MIT
//! In-viewer search (T-5.3.6, design.md §9.6): a byte search for the
//! needle encoded in the file's encoding, streamed over 1 MiB windows
//! with `needle.len() - 1` bytes of overlap so a hit straddling two
//! windows is still found. Optional ASCII case folding (both the needle
//! and each window are lowercased; in UTF-16 the zero high bytes fold to
//! themselves, so it works there too). For UTF-16 a hit must start on a
//! code-unit boundary. Cancelled between windows through `cancel`.

use std::sync::atomic::{AtomicBool, Ordering};

use memchr::memmem;

use super::source::{ByteSource, CHUNK};

pub(crate) struct SearchSpec {
    pub(crate) needle: Vec<u8>,
    pub(crate) fold_case: bool,
    /// Code-unit size (2 for UTF-16) and BOM length, for alignment.
    pub(crate) unit: u64,
    pub(crate) bom: u64,
}

impl SearchSpec {
    fn prepared_needle(&self) -> Vec<u8> {
        if self.fold_case {
            self.needle.to_ascii_lowercase()
        } else {
            self.needle.clone()
        }
    }

    fn aligned(&self, offset: u64) -> bool {
        self.unit <= 1 || (offset.saturating_sub(self.bom)).is_multiple_of(self.unit)
    }
}

fn fold(spec: &SearchSpec, mut window: Vec<u8>) -> Vec<u8> {
    if spec.fold_case {
        window.make_ascii_lowercase();
    }
    window
}

/// The first hit starting at or after `from`.
pub(crate) fn search_forward<S: ByteSource>(
    src: &mut S,
    spec: &SearchSpec,
    from: u64,
    cancel: &AtomicBool,
    mut progress: impl FnMut(f32),
) -> std::io::Result<Option<u64>> {
    let needle = spec.prepared_needle();
    if needle.is_empty() {
        return Ok(None);
    }
    let len = src.len();
    let finder = memmem::Finder::new(&needle);
    let overlap = needle.len() - 1;
    let mut pos = from;
    while pos < len {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let window = fold(spec, src.read(pos, CHUNK as usize + overlap)?);
        if let Some(m) = finder
            .find_iter(&window)
            .find(|&m| spec.aligned(pos + m as u64))
        {
            return Ok(Some(pos + m as u64));
        }
        if window.len() < CHUNK as usize + overlap {
            break;
        }
        pos += CHUNK;
        progress(pos as f32 / len.max(1) as f32);
    }
    Ok(None)
}

/// The last hit starting strictly before `before`.
pub(crate) fn search_backward<S: ByteSource>(
    src: &mut S,
    spec: &SearchSpec,
    before: u64,
    cancel: &AtomicBool,
    mut progress: impl FnMut(f32),
) -> std::io::Result<Option<u64>> {
    let needle = spec.prepared_needle();
    if needle.is_empty() {
        return Ok(None);
    }
    let len = src.len();
    let finder = memmem::FinderRev::new(&needle);
    let overlap = needle.len() as u64 - 1;
    // Window `[lo, hi)`: hits starting in `[lo, before)` may run to
    // `before + overlap`.
    let mut hi = before.saturating_add(overlap).min(len);
    loop {
        if hi == 0 || cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let lo = hi.saturating_sub(CHUNK + overlap);
        let window = fold(spec, src.read(lo, (hi - lo) as usize)?);
        if let Some(m) = finder
            .rfind_iter(&window)
            .find(|&m| lo + (m as u64) < before && spec.aligned(lo + m as u64))
        {
            return Ok(Some(lo + m as u64));
        }
        if lo == 0 {
            return Ok(None);
        }
        hi = lo + overlap;
        progress(1.0 - hi as f32 / len.max(1) as f32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(needle: &[u8], fold_case: bool) -> SearchSpec {
        SearchSpec {
            needle: needle.to_vec(),
            fold_case,
            unit: 1,
            bom: 0,
        }
    }

    #[test]
    fn finds_hits_across_window_boundaries_both_ways() {
        let mut src = vec![b'.'; 3 * CHUNK as usize];
        let at = CHUNK as usize - 3; // "needle" straddles the first boundary
        src[at..at + 6].copy_from_slice(b"needle");
        let at2 = 2 * CHUNK as usize + 10;
        src[at2..at2 + 6].copy_from_slice(b"NEEDLE");
        let cancel = AtomicBool::new(false);
        let s = spec(b"needle", false);
        assert_eq!(
            search_forward(&mut src, &s, 0, &cancel, |_| {}).unwrap(),
            Some(at as u64)
        );
        assert_eq!(
            search_forward(&mut src, &s, at as u64 + 1, &cancel, |_| {}).unwrap(),
            None,
            "case-sensitive: the upper-case one is not a hit"
        );
        let folded = spec(b"Needle", true);
        assert_eq!(
            search_forward(&mut src, &folded, at as u64 + 1, &cancel, |_| {}).unwrap(),
            Some(at2 as u64)
        );
        let end = src.len() as u64;
        assert_eq!(
            search_backward(&mut src, &folded, end, &cancel, |_| {}).unwrap(),
            Some(at2 as u64)
        );
        assert_eq!(
            search_backward(&mut src, &folded, at2 as u64, &cancel, |_| {}).unwrap(),
            Some(at as u64)
        );
        assert_eq!(
            search_backward(&mut src, &folded, at as u64, &cancel, |_| {}).unwrap(),
            None
        );
    }

    #[test]
    fn utf16_hits_must_be_unit_aligned_and_cancel_stops_early() {
        // LE "ab" = 61 00 62 00; the needle "\0a" (BE-looking) matches
        // at odd offset 1 only.
        let mut src = b"a\0b\0a\0".to_vec();
        let s = SearchSpec {
            needle: b"\0a".to_vec(),
            fold_case: false,
            unit: 2,
            bom: 0,
        };
        let cancel = AtomicBool::new(false);
        assert_eq!(
            search_forward(&mut src, &s, 0, &cancel, |_| {}).unwrap(),
            None
        );
        let s2 = SearchSpec {
            needle: b"a\0".to_vec(),
            ..s
        };
        assert_eq!(
            search_forward(&mut src, &s2, 1, &cancel, |_| {}).unwrap(),
            Some(4)
        );

        let mut big = vec![b'x'; 2 * CHUNK as usize + 5];
        big.extend(b"end");
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            search_forward(&mut big, &spec(b"end", false), 0, &cancelled, |_| {}).unwrap(),
            None
        );
        assert_eq!(
            search_forward(&mut big, &spec(b"", false), 0, &cancel, |_| {}).unwrap(),
            None
        );
    }
}
