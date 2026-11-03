// SPDX-License-Identifier: MIT
//! Text mode's engine (T-5.3.6, design.md §9.6): encoding detection,
//! the row pager, wrapping and page assembly. Pure Rust -- no gpui, and
//! every byte comes through [`ByteSource`], so the unit tests run it
//! over a `Vec<u8>`.
//!
//! **Rows are derived locally, never from a whole-file index.** A row is
//! a byte range: from a row start, the next newline within `chop`
//! (4 KiB) bytes ends it; failing that, the row is chopped at the next
//! multiple of `chop`. Going backwards from a row start, the last
//! newline in the preceding `chop` bytes and the preceding multiple
//! decide the previous row. The two walks provably agree (see
//! `prev_row_start`'s doc comment for the argument, and the property
//! test at the bottom), which is what lets End, `50%` and a search hit
//! land anywhere in a 5 GB file in one read, and what turns a 200 MB
//! single-line JSON into fifty thousand ordinary rows instead of one
//! string. A *line* longer than `chop` therefore shows as several rows;
//! shorter lines are never split (except by wrapping). A UTF-8
//! character cut by a chop is repaired when the row is decoded
//! (`Pager::row_bytes`), not by moving the boundary.
//!
//! Wrap mode splits a row's decoded text into pieces of `cols`
//! characters; a page position is then `(row start, piece)`.

use std::collections::HashMap;
use std::ops::Range;

use encoding_rs::{
    Encoding, KOI8_R, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1251, WINDOWS_1252, WINDOWS_1255,
};
use memchr::{memchr, memchr_iter, memrchr, memrchr_iter};

use super::source::ByteSource;

/// The chop size -- see the module doc comment. Even, so UTF-16 code
/// units never straddle a chop.
pub(crate) const DEFAULT_CHOP: u64 = 4096;
pub(crate) const TAB_WIDTH: usize = 8;
/// How much of the file's head detection looks at.
pub(crate) const HEAD_BYTES: usize = 64 * 1024;

/// The encodings the manual override cycles through
/// (`view.set_encoding`): UTF-8 first, then the single-byte code pages
/// Total Commander's "ANSI" would cover for Western, Cyrillic and Hebrew
/// text, then UTF-16 both ways.
pub(crate) const ENCODING_CYCLE: [&Encoding; 7] = [
    UTF_8,
    WINDOWS_1252,
    WINDOWS_1251,
    WINDOWS_1255,
    KOI8_R,
    UTF_16LE,
    UTF_16BE,
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct TextEncoding(pub(crate) &'static Encoding);

impl TextEncoding {
    pub(crate) fn name(self) -> &'static str {
        self.0.name()
    }

    pub(crate) fn next(self) -> Self {
        let pos = ENCODING_CYCLE
            .iter()
            .position(|e| *e == self.0)
            .unwrap_or(ENCODING_CYCLE.len() - 1);
        Self(ENCODING_CYCLE[(pos + 1) % ENCODING_CYCLE.len()])
    }

    pub(crate) fn is_utf16(self) -> bool {
        self.0 == UTF_16LE || self.0 == UTF_16BE
    }

    fn is_utf8(self) -> bool {
        self.0 == UTF_8
    }

    /// Bytes per code unit: 2 for UTF-16, else 1.
    pub(crate) fn unit(self) -> u64 {
        if self.is_utf16() { 2 } else { 1 }
    }

    pub(crate) fn decode(self, bytes: &[u8]) -> String {
        self.0.decode_without_bom_handling(bytes).0.into_owned()
    }

    /// `text` in this encoding, for searching. `encoding_rs` encodes to
    /// UTF-8 for the UTF-16 encodings by design, so those are done here.
    pub(crate) fn encode(self, text: &str) -> Vec<u8> {
        if self.0 == UTF_16LE {
            text.encode_utf16().flat_map(u16::to_le_bytes).collect()
        } else if self.0 == UTF_16BE {
            text.encode_utf16().flat_map(u16::to_be_bytes).collect()
        } else {
            self.0.encode(text).0.into_owned()
        }
    }
}

/// What the head of a file says about it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Detected {
    pub(crate) encoding: TextEncoding,
    /// Bytes of byte-order mark to skip; the first row starts here.
    pub(crate) bom: u64,
    /// NUL bytes in something that isn't UTF-16: auto mode shows hex.
    pub(crate) binary: bool,
}

/// BOM, then "is the head valid UTF-8", then a UTF-16 heuristic (ASCII
/// text in UTF-16 has a NUL every other byte), else windows-1252 -- the
/// "ANSI" fallback Total Commander's Lister uses. `chardetng` would do
/// better on legacy Cyrillic/CJK text; it is a new crate and left as a
/// follow-up, the manual override covers the gap.
pub(crate) fn detect(head: &[u8]) -> Detected {
    if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Detected {
            encoding: TextEncoding(UTF_8),
            bom: 3,
            binary: false,
        };
    }
    if head.starts_with(&[0xFF, 0xFE]) {
        return Detected {
            encoding: TextEncoding(UTF_16LE),
            bom: 2,
            binary: false,
        };
    }
    if head.starts_with(&[0xFE, 0xFF]) {
        return Detected {
            encoding: TextEncoding(UTF_16BE),
            bom: 2,
            binary: false,
        };
    }
    let has_nul = memchr(0, head).is_some();
    if !has_nul {
        let utf8_ok = match std::str::from_utf8(head) {
            Ok(_) => true,
            // A sequence cut by the end of the sampled head is fine.
            Err(e) => e.error_len().is_none() && head.len() >= HEAD_BYTES,
        };
        let encoding = if utf8_ok {
            TextEncoding(UTF_8)
        } else {
            TextEncoding(WINDOWS_1252)
        };
        return Detected {
            encoding,
            bom: 0,
            binary: false,
        };
    }
    // NULs: UTF-16 without a BOM, or binary.
    let sample = &head[..head.len().min(4096)];
    let (mut even, mut odd) = (0usize, 0usize);
    for (i, b) in sample.iter().enumerate() {
        if *b == 0 {
            if i % 2 == 0 {
                even += 1;
            } else {
                odd += 1;
            }
        }
    }
    let threshold = sample.len() / 8;
    if odd > threshold && even < odd / 8 {
        return Detected {
            encoding: TextEncoding(UTF_16LE),
            bom: 0,
            binary: false,
        };
    }
    if even > threshold && odd < even / 8 {
        return Detected {
            encoding: TextEncoding(UTF_16BE),
            bom: 0,
            binary: false,
        };
    }
    Detected {
        encoding: TextEncoding(WINDOWS_1252),
        bom: 0,
        binary: true,
    }
}

/// Index just past the first newline unit in `hay` (which starts at
/// absolute offset `base`), or `None`.
fn find_newline(enc: TextEncoding, hay: &[u8], base: u64, bom: u64) -> Option<usize> {
    if !enc.is_utf16() {
        return memchr(b'\n', hay).map(|p| p + 1);
    }
    let aligned = |p: usize| (base + p as u64 - bom).is_multiple_of(2);
    if enc.0 == UTF_16LE {
        memchr_iter(b'\n', hay)
            .find(|&p| aligned(p) && hay.get(p + 1) == Some(&0))
            .map(|p| p + 2)
    } else {
        memchr_iter(b'\n', hay)
            .find(|&p| p >= 1 && aligned(p - 1) && hay[p - 1] == 0)
            .map(|p| p + 1)
    }
}

/// Every index just past a newline unit in `hay` (which starts at the
/// unit-aligned absolute offset `base`) -- what the line index and the
/// line counter iterate. For UTF-16 a `\n` must sit at a code-unit
/// boundary with a zero partner; a U+0A0A or the low byte of U+2A0A is
/// not a newline.
pub(crate) fn newline_ends<'a>(
    enc: TextEncoding,
    hay: &'a [u8],
    base: u64,
    bom: u64,
) -> impl Iterator<Item = usize> + 'a {
    let utf16 = enc.is_utf16();
    let le = enc.0 == UTF_16LE;
    memchr_iter(b'\n', hay).filter_map(move |p| {
        if !utf16 {
            return Some(p + 1);
        }
        if le {
            ((base + p as u64 - bom).is_multiple_of(2) && hay.get(p + 1) == Some(&0))
                .then_some(p + 2)
        } else {
            (p >= 1 && (base + p as u64 - 1 - bom).is_multiple_of(2) && hay[p - 1] == 0)
                .then_some(p + 1)
        }
    })
}

/// Index just past the *last* newline unit in `hay`, or `None`.
fn rfind_newline(enc: TextEncoding, hay: &[u8], base: u64, bom: u64) -> Option<usize> {
    if !enc.is_utf16() {
        return memrchr(b'\n', hay).map(|p| p + 1);
    }
    let aligned = |p: usize| (base + p as u64 - bom).is_multiple_of(2);
    if enc.0 == UTF_16LE {
        memrchr_iter(b'\n', hay)
            .find(|&p| aligned(p) && hay.get(p + 1) == Some(&0))
            .map(|p| p + 2)
    } else {
        memrchr_iter(b'\n', hay)
            .find(|&p| p >= 1 && aligned(p - 1) && hay[p - 1] == 0)
            .map(|p| p + 1)
    }
}

/// The row-boundary rules of the module doc comment.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pager {
    pub(crate) enc: TextEncoding,
    pub(crate) bom: u64,
    pub(crate) chop: u64,
}

impl Pager {
    pub(crate) fn new(detected: Detected) -> Self {
        Self {
            enc: detected.encoding,
            bom: detected.bom,
            chop: DEFAULT_CHOP,
        }
    }

    /// The last chop point (multiple of `chop`) at or before `at`, or
    /// the BOM when there is none above it.
    fn chop_at_or_before(&self, at: u64) -> u64 {
        ((at / self.chop) * self.chop).max(self.bom)
    }

    /// The row starting at `start` (which must be a row start), or
    /// `None` at the end of the file: up to and including the first
    /// newline within `chop` bytes, else up to the next chop point.
    pub(crate) fn next_row<S: ByteSource>(
        &self,
        src: &mut S,
        start: u64,
    ) -> std::io::Result<Option<Range<u64>>> {
        let len = src.len();
        if start >= len {
            return Ok(None);
        }
        let buf = src.read(start, self.chop as usize)?;
        if let Some(nl) = find_newline(self.enc, &buf, start, self.bom) {
            return Ok(Some(start..start + nl as u64));
        }
        let chop = (start / self.chop + 1) * self.chop;
        Ok(Some(start..chop.min(len)))
    }

    /// Whether the code unit just before `top` is a newline.
    fn newline_before<S: ByteSource>(&self, src: &mut S, top: u64) -> std::io::Result<bool> {
        let u = self.enc.unit();
        if top < self.bom + u {
            return Ok(false);
        }
        let bytes = src.read(top - u, u as usize)?;
        Ok(match (self.enc.is_utf16(), self.enc.0 == UTF_16LE) {
            (false, _) => bytes.first() == Some(&b'\n'),
            (true, true) => bytes == [b'\n', 0],
            (true, false) => bytes == [0, b'\n'],
        })
    }

    /// The start of the row that ends at `top` (a row start), or `None`
    /// at the beginning of the file.
    ///
    /// Why this agrees with [`Self::next_row`]. Let `ls` be the start of
    /// the line containing the row and `c` the last chop point below
    /// `top`. The forward walk chops a line at a chop point exactly when
    /// the `chop` bytes after the previous row start hold no newline.
    /// - If `top` is a chop point or the end of the file, the row ended
    ///   without a newline, so `c` is a boundary whenever it lies inside
    ///   the line: the row starts at `max(ls, c)`.
    /// - If `top` ends with a newline, `c` is a boundary when it lies
    ///   inside the line *and* the newline is at least `chop` bytes
    ///   after the boundary before `c` -- which is `ls`, since the chop
    ///   before `c` is `c - chop <= top - 1 - chop`. So the row starts
    ///   at `c` when `ls < c` and `ls <= top - unit - chop`, else `ls`.
    ///
    /// `ls` only matters when it is within `chop` bytes of `top`, which
    /// is exactly the window scanned here; when it is further back the
    /// BOM stands in and both comparisons come out the same.
    pub(crate) fn prev_row_start<S: ByteSource>(
        &self,
        src: &mut S,
        top: u64,
    ) -> std::io::Result<Option<u64>> {
        if top <= self.bom {
            return Ok(None);
        }
        let hi = top - self.enc.unit();
        let lo = hi.saturating_sub(self.chop).max(self.bom);
        let mut ls = self.bom;
        if hi > lo {
            let buf = src.read(lo, (hi - lo) as usize)?;
            if let Some(nl) = rfind_newline(self.enc, &buf, lo, self.bom) {
                ls = lo + nl as u64;
            }
        }
        let c = self.chop_at_or_before(top - 1);
        if c <= ls {
            return Ok(Some(ls));
        }
        if self.newline_before(src, top)? && ls > hi.saturating_sub(self.chop) {
            // A line shorter than `chop` that crosses `c`: never chopped.
            return Ok(Some(ls));
        }
        Ok(Some(c))
    }

    /// The row containing byte `offset` (for a search hit or a byte
    /// goto), or `None` past the end. The line start, when within `chop`
    /// bytes, is a row start; otherwise the last chop point at or
    /// before `offset` is one (the line runs more than `chop` bytes on
    /// either side of it). Either is at most two rows away.
    pub(crate) fn row_containing<S: ByteSource>(
        &self,
        src: &mut S,
        offset: u64,
    ) -> std::io::Result<Option<Range<u64>>> {
        let len = src.len();
        if offset >= len {
            return Ok(None);
        }
        let offset = offset.max(self.bom);
        let lo = offset.saturating_sub(self.chop).max(self.bom);
        let mut start = None;
        if offset > lo {
            let buf = src.read(lo, (offset - lo) as usize)?;
            start = rfind_newline(self.enc, &buf, lo, self.bom).map(|nl| lo + nl as u64);
        }
        let mut start = start.unwrap_or_else(|| self.chop_at_or_before(offset));
        let mut last = None;
        for _ in 0..8 {
            let Some(row) = self.next_row(src, start)? else {
                break;
            };
            if row.contains(&offset) {
                return Ok(Some(row));
            }
            start = row.end;
            last = Some(row);
        }
        Ok(last)
    }

    /// The bytes of `row` as they should be decoded: a UTF-8 character
    /// split by a chop is completed from the following bytes at the end
    /// and dropped at the start (the previous row showed it). Returns
    /// the bytes and how many leading bytes were skipped.
    fn row_bytes<S: ByteSource>(
        &self,
        src: &mut S,
        row: &Range<u64>,
    ) -> std::io::Result<(Vec<u8>, usize)> {
        let mut bytes = src.read(row.start, (row.end - row.start) as usize)?;
        if !self.enc.is_utf8() {
            return Ok((bytes, 0));
        }
        let skip = if row.start > self.bom {
            bytes
                .iter()
                .take(3)
                .take_while(|b| **b & 0xC0 == 0x80)
                .count()
        } else {
            0
        };
        if skip > 0 {
            bytes.drain(..skip);
        }
        // An incomplete sequence at the end: a lead byte within the
        // last three that promises more bytes than remain.
        let n = bytes.len();
        for back in 1..=n.min(3) {
            let b = bytes[n - back];
            if b & 0xC0 == 0x80 {
                continue;
            }
            if b & 0xC0 == 0xC0 {
                let want = if b >= 0xF0 {
                    4
                } else if b >= 0xE0 {
                    3
                } else {
                    2
                };
                if want > back && row.end < src.len() {
                    let more = src.read(row.end, want - back)?;
                    bytes.extend_from_slice(&more);
                }
            }
            break;
        }
        Ok((bytes, skip))
    }
}

/// A row's text as shown: one trailing newline (`\n` or `\r\n`)
/// stripped, tabs expanded to [`TAB_WIDTH`] stops, other control
/// characters shown as their Control Pictures glyph so they take a cell
/// instead of vanishing.
pub(crate) fn display_text(raw: &str) -> String {
    let raw = raw.strip_suffix('\n').unwrap_or(raw);
    let raw = raw.strip_suffix('\r').unwrap_or(raw);
    let mut out = String::with_capacity(raw.len());
    let mut col = 0usize;
    for ch in raw.chars() {
        match ch {
            '\t' => {
                let next = (col / TAB_WIDTH + 1) * TAB_WIDTH;
                for _ in col..next {
                    out.push(' ');
                }
                col = next;
            }
            c if (c as u32) < 0x20 => {
                out.push(char::from_u32(0x2400 + c as u32).unwrap_or('\u{FFFD}'));
                col += 1;
            }
            '\u{7F}' => {
                out.push('\u{2421}');
                col += 1;
            }
            c => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

/// Byte ranges of `text` cut every `cols` characters; always at least
/// one piece (empty for an empty row).
pub(crate) fn wrap_pieces(text: &str, cols: usize) -> Vec<Range<usize>> {
    let cols = cols.max(1);
    let mut pieces = Vec::new();
    let mut piece_start = 0;
    let mut count = 0;
    for (i, _) in text.char_indices() {
        if count == cols {
            pieces.push(piece_start..i);
            piece_start = i;
            count = 0;
        }
        count += 1;
    }
    pieces.push(piece_start..text.len());
    pieces
}

/// A page position: a row start and the wrap piece shown first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Position {
    pub(crate) row_start: u64,
    pub(crate) piece: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VisualRow {
    pub(crate) row: Range<u64>,
    pub(crate) piece: usize,
    pub(crate) text: String,
    /// Byte range of `text` to highlight (a search hit).
    pub(crate) highlight: Option<Range<usize>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Page {
    pub(crate) top: Position,
    pub(crate) rows: Vec<VisualRow>,
    pub(crate) at_start: bool,
    pub(crate) at_end: bool,
}

struct Decoded {
    row: Range<u64>,
    /// The decoded bytes (seam-repaired, see `Pager::row_bytes`) and
    /// how many of the row's leading bytes they leave out.
    raw: Vec<u8>,
    skip: usize,
    text: String,
    pieces: Vec<Range<usize>>,
    /// Character columns of the highlight within `text`.
    highlight: Option<Range<usize>>,
}

impl Decoded {
    /// Display columns before absolute byte `abs` on this row.
    fn columns_before(&self, enc: TextEncoding, abs: u64) -> usize {
        let cut = (abs.clamp(self.row.start, self.row.end) - self.row.start) as usize;
        let cut = cut.saturating_sub(self.skip).min(self.raw.len());
        display_text(&enc.decode(&self.raw[..cut])).chars().count()
    }
}

/// One request's worth of row walking: decodes each row once.
pub(crate) struct Walker<'a, S: ByteSource> {
    pager: Pager,
    src: &'a mut S,
    /// `None`: no wrapping.
    cols: Option<usize>,
    highlight: Option<Range<u64>>,
    cache: HashMap<u64, Decoded>,
}

impl<'a, S: ByteSource> Walker<'a, S> {
    pub(crate) fn new(
        pager: Pager,
        src: &'a mut S,
        cols: Option<usize>,
        highlight: Option<Range<u64>>,
    ) -> Self {
        Self {
            pager,
            src,
            cols,
            highlight,
            cache: HashMap::new(),
        }
    }

    pub(crate) fn start(&self) -> Position {
        Position {
            row_start: self.pager.bom,
            piece: 0,
        }
    }

    fn decoded(&mut self, start: u64) -> std::io::Result<Option<&Decoded>> {
        if !self.cache.contains_key(&start) {
            let Some(row) = self.pager.next_row(self.src, start)? else {
                return Ok(None);
            };
            let (raw, skip) = self.pager.row_bytes(self.src, &row)?;
            let enc = self.pager.enc;
            let text = display_text(&enc.decode(&raw));
            let pieces = match self.cols {
                Some(cols) => wrap_pieces(&text, cols),
                None => std::iter::once(0..text.len()).collect(),
            };
            let mut decoded = Decoded {
                row: row.clone(),
                raw,
                skip,
                text,
                pieces,
                highlight: None,
            };
            if let Some(hl) = &self.highlight
                && hl.start < row.end
                && hl.end > row.start
            {
                let total = decoded.text.chars().count();
                let a = decoded.columns_before(enc, hl.start).min(total);
                let b = decoded.columns_before(enc, hl.end).clamp(a, total);
                decoded.highlight = (b > a).then_some(a..b);
            }
            self.cache.insert(start, decoded);
        }
        Ok(self.cache.get(&start))
    }

    /// `n` visual rows forward, clamped at the last one.
    pub(crate) fn advance(&mut self, mut pos: Position, n: usize) -> std::io::Result<Position> {
        for _ in 0..n {
            let Some(d) = self.decoded(pos.row_start)? else {
                break;
            };
            if pos.piece + 1 < d.pieces.len() {
                pos.piece += 1;
            } else {
                let next = d.row.end;
                if self.decoded(next)?.is_none() {
                    break;
                }
                pos = Position {
                    row_start: next,
                    piece: 0,
                };
            }
        }
        Ok(pos)
    }

    /// `n` visual rows back, clamped at the first one.
    pub(crate) fn retreat(&mut self, mut pos: Position, n: usize) -> std::io::Result<Position> {
        for _ in 0..n {
            if pos.piece > 0 {
                pos.piece -= 1;
                continue;
            }
            let Some(prev) = self.pager.prev_row_start(self.src, pos.row_start)? else {
                break;
            };
            let pieces = self.decoded(prev)?.map_or(1, |d| d.pieces.len());
            pos = Position {
                row_start: prev,
                piece: pieces - 1,
            };
        }
        Ok(pos)
    }

    /// The topmost position from which `count` visual rows reach the
    /// end of the file -- where End lands, and the floor for scrolling.
    pub(crate) fn end_top(&mut self, count: usize) -> std::io::Result<Position> {
        let len = self.src.len();
        let last_start = match self.pager.prev_row_start(self.src, len)? {
            Some(s) => s,
            None => return Ok(self.start()),
        };
        let pieces = self.decoded(last_start)?.map_or(1, |d| d.pieces.len());
        let last = Position {
            row_start: last_start,
            piece: pieces - 1,
        };
        self.retreat(last, count.saturating_sub(1))
    }

    /// `top` moved by `delta` visual rows, never past [`Self::end_top`].
    pub(crate) fn scroll(
        &mut self,
        top: Position,
        delta: i64,
        count: usize,
    ) -> std::io::Result<Position> {
        if delta < 0 {
            return self.retreat(top, delta.unsigned_abs() as usize);
        }
        let moved = self.advance(top, delta as usize)?;
        let floor = self.end_top(count)?;
        Ok(moved.min(floor))
    }

    /// The position whose row contains `offset`, piece included.
    pub(crate) fn position_at(&mut self, offset: u64) -> std::io::Result<Position> {
        let Some(row) = self.pager.row_containing(self.src, offset)? else {
            return self.end_top(1);
        };
        let start = row.start;
        let cols = self.cols.unwrap_or(usize::MAX).max(1);
        let enc = self.pager.enc;
        let (col, pieces) = match self.decoded(start)? {
            Some(d) => (d.columns_before(enc, offset), d.pieces.len()),
            None => (0, 1),
        };
        Ok(Position {
            row_start: start,
            piece: (col / cols).min(pieces.saturating_sub(1)),
        })
    }

    pub(crate) fn page(&mut self, top: Position, count: usize) -> std::io::Result<Page> {
        let mut rows = Vec::with_capacity(count);
        let mut pos = top;
        let mut at_end = false;
        let cols = self.cols.unwrap_or(usize::MAX).max(1);
        while rows.len() < count {
            let Some(d) = self.decoded(pos.row_start)? else {
                at_end = true;
                break;
            };
            let piece = pos.piece.min(d.pieces.len() - 1);
            let range = d.pieces[piece].clone();
            let text = d.text[range.clone()].to_string();
            let highlight = d.highlight.clone().and_then(|hl| {
                let piece_cols =
                    piece.saturating_mul(cols)..piece.saturating_mul(cols).saturating_add(cols);
                let a = hl.start.max(piece_cols.start);
                let b = hl.end.min(piece_cols.end);
                if b <= a {
                    return None;
                }
                let byte_at = |col: usize| {
                    text.char_indices()
                        .nth(col - piece_cols.start)
                        .map_or(text.len(), |(i, _)| i)
                };
                Some(byte_at(a)..byte_at(b))
            });
            rows.push(VisualRow {
                row: d.row.clone(),
                piece,
                text,
                highlight,
            });
            if piece + 1 < d.pieces.len() {
                pos.piece = piece + 1;
            } else {
                pos = Position {
                    row_start: d.row.end,
                    piece: 0,
                };
            }
        }
        if !at_end && self.decoded(pos.row_start)?.is_none() {
            at_end = true;
        }
        Ok(Page {
            top,
            rows,
            at_start: top == self.start(),
            at_end,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pager(enc: &'static Encoding, bom: u64, chop: u64) -> Pager {
        Pager {
            enc: TextEncoding(enc),
            bom,
            chop,
        }
    }

    fn forward_rows(p: &Pager, src: &mut Vec<u8>) -> Vec<Range<u64>> {
        let mut rows = Vec::new();
        let mut start = p.bom;
        while let Some(r) = p.next_row(src, start).unwrap() {
            assert!(r.end > r.start, "empty row at {start}");
            start = r.end;
            rows.push(r);
        }
        rows
    }

    fn backward_rows(p: &Pager, src: &mut Vec<u8>) -> Vec<Range<u64>> {
        let mut rows = Vec::new();
        let mut top = src.len() as u64;
        while let Some(s) = p.prev_row_start(src, top).unwrap() {
            assert!(s < top, "non-advancing backward step at {top}");
            rows.push(s..top);
            top = s;
        }
        rows.reverse();
        rows
    }

    #[test]
    fn detects_boms_utf8_utf16_and_falls_back_to_windows_1252() {
        assert_eq!(
            detect(b"\xEF\xBB\xBFhi"),
            Detected {
                encoding: TextEncoding(UTF_8),
                bom: 3,
                binary: false
            }
        );
        assert_eq!(detect(b"\xFF\xFEh\0i\0").encoding, TextEncoding(UTF_16LE));
        assert_eq!(detect(b"\xFE\xFF\0h\0i").encoding, TextEncoding(UTF_16BE));
        assert_eq!(detect("héllo\n".as_bytes()).encoding, TextEncoding(UTF_8));
        assert_eq!(detect(b"h\xE9llo\n").encoding, TextEncoding(WINDOWS_1252));
        let le: Vec<u8> = "plain ascii text, lots of it\n"
            .repeat(20)
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let d = detect(&le);
        assert_eq!(
            (d.encoding, d.bom, d.binary),
            (TextEncoding(UTF_16LE), 0, false)
        );
        let be: Vec<u8> = "plain ascii text, lots of it\n"
            .repeat(20)
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        assert_eq!(detect(&be).encoding, TextEncoding(UTF_16BE));
        let bin = detect(b"\x7FELF\x02\x01\x01\0\0\0\0\0\0\0\0\0\x03\0>\0");
        assert!(bin.binary);
        assert!(!detect(b"").binary);
    }

    #[test]
    fn encoding_cycle_wraps_and_utf16_round_trips() {
        let mut e = TextEncoding(UTF_8);
        for _ in 0..ENCODING_CYCLE.len() {
            e = e.next();
        }
        assert_eq!(e, TextEncoding(UTF_8));
        let le = TextEncoding(UTF_16LE);
        assert_eq!(le.encode("ab"), b"a\0b\0");
        assert_eq!(le.decode(b"a\0b\0"), "ab");
        assert_eq!(TextEncoding(UTF_16BE).encode("ab"), b"\0a\0b");
        assert_eq!(TextEncoding(WINDOWS_1252).encode("é"), b"\xE9");
    }

    #[test]
    fn short_lines_are_never_chopped_and_long_lines_chop_at_multiples() {
        let p = pager(UTF_8, 0, 16);
        // Line crossing offset 16 but shorter than 16 bytes: intact.
        let mut src = b"0123456789\nabcdefghij\nxy".to_vec();
        assert_eq!(forward_rows(&p, &mut src), vec![0..11, 11..22, 22..24]);
        // A 40-byte line starting at 5: chopped at 16 and 32.
        let mut src = b"abcd\n".to_vec();
        src.extend(std::iter::repeat_n(b'x', 40));
        src.extend(b"\nend\n");
        let rows = forward_rows(&p, &mut src);
        assert_eq!(rows, vec![0..5, 5..16, 16..32, 32..46, 46..50]);
        assert_eq!(backward_rows(&p, &mut src), rows);
    }

    #[test]
    fn a_character_cut_by_a_chop_is_repaired_when_decoding() {
        let p = pager(UTF_8, 0, 16);
        // "é" is 2 bytes; place one so it straddles offset 16 inside a
        // long line.
        let mut src = b"123456789012345".to_vec(); // 15 bytes
        src.extend("é".as_bytes()); // bytes 15,16
        src.extend(std::iter::repeat_n(b'z', 20));
        let rows = forward_rows(&p, &mut src);
        assert_eq!(rows[0], 0..16, "the boundary itself stays at the multiple");
        assert_eq!(backward_rows(&p, &mut src), rows);
        let (first, skip) = p.row_bytes(&mut src, &rows[0]).unwrap();
        assert_eq!(
            (std::str::from_utf8(&first).unwrap(), skip),
            ("123456789012345é", 0)
        );
        let (second, skip) = p.row_bytes(&mut src, &rows[1]).unwrap();
        assert_eq!(
            (std::str::from_utf8(&second).unwrap(), skip),
            ("zzzzzzzzzzzzzzz", 1)
        );
        let mut w = Walker::new(p, &mut src, None, None);
        let page = w.page(w.start(), 3).unwrap();
        assert_eq!(page.rows[0].text, "123456789012345é");
        assert_eq!(page.rows[1].text, "zzzzzzzzzzzzzzz");
    }

    #[test]
    fn forward_and_backward_walks_agree_on_random_input() {
        // A tiny deterministic LCG; no rand dependency here.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        let alphabet: &[&str] = &["a", "b", " ", "\n", "\r\n", "é", "日", "\t", "😀", "\n\n"];
        for case in 0..400 {
            let len = (next() % 200) as usize;
            let mut s = String::new();
            while s.len() < len {
                let pick = next() as usize % alphabet.len();
                // Bias towards long runs without newlines every few cases.
                if case % 3 == 0 && alphabet[pick].contains('\n') && next() % 4 != 0 {
                    continue;
                }
                s.push_str(alphabet[pick]);
            }
            let chop = [8u64, 16, 32][case % 3];
            let mut src = s.into_bytes();
            let p = pager(UTF_8, 0, chop);
            let f = forward_rows(&p, &mut src);
            let b = backward_rows(&p, &mut src);
            assert_eq!(f, b, "case {case}: {:?}", String::from_utf8_lossy(&src));
            let total: u64 = f.iter().map(|r| r.end - r.start).sum();
            assert_eq!(total, src.len() as u64);
            for (i, r) in f.iter().enumerate() {
                assert_eq!(p.row_containing(&mut src, r.start).unwrap().unwrap(), *r);
                assert_eq!(
                    p.row_containing(&mut src, r.end - 1).unwrap().unwrap(),
                    *r,
                    "case {case} row {i}"
                );
            }
        }
    }

    #[test]
    fn utf16_rows_split_on_aligned_newlines_only() {
        let text = "ab\ncd\n\u{0A0A}e\n"; // U+0A0A is "\n\n" misaligned in LE
        let mut le = vec![0xFF, 0xFE];
        le.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        let p = pager(UTF_16LE, 2, 16);
        let rows = forward_rows(&p, &mut le);
        assert_eq!(rows, vec![2..8, 8..14, 14..20]);
        assert_eq!(backward_rows(&p, &mut le), rows);
        let mut be = vec![0xFE, 0xFF];
        be.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
        let p = pager(UTF_16BE, 2, 16);
        let rows = forward_rows(&p, &mut be);
        assert_eq!(rows, vec![2..8, 8..14, 14..20]);
        assert_eq!(backward_rows(&p, &mut be), rows);
        // A long UTF-16 line chops on even boundaries.
        let mut long = vec![0xFF, 0xFE];
        long.extend("x".repeat(40).encode_utf16().flat_map(u16::to_le_bytes));
        let p = pager(UTF_16LE, 2, 16);
        let rows = forward_rows(&p, &mut long);
        assert_eq!(rows, vec![2..16, 16..32, 32..48, 48..64, 64..80, 80..82]);
        assert_eq!(backward_rows(&p, &mut long), rows);
    }

    #[test]
    fn display_text_strips_one_newline_expands_tabs_and_pictures_controls() {
        assert_eq!(display_text("abc\r\n"), "abc");
        assert_eq!(display_text("abc\n\n"), "abc\u{240A}");
        assert_eq!(display_text("a\tb"), "a       b");
        assert_eq!(display_text("12345678\tb"), "12345678        b");
        assert_eq!(display_text("a\x01b\x7F"), "a\u{2401}b\u{2421}");
    }

    #[test]
    fn wrap_pieces_cut_by_characters_not_bytes() {
        assert_eq!(wrap_pieces("", 4), vec![0..0]);
        assert_eq!(wrap_pieces("abcdefgh", 4), vec![0..4, 4..8]);
        assert_eq!(wrap_pieces("abcdefghi", 4), vec![0..4, 4..8, 8..9]);
        // "日" is 3 bytes.
        assert_eq!(wrap_pieces("日日日", 2), vec![0..6, 6..9]);
        assert_eq!(wrap_pieces("abc", 0), vec![0..1, 1..2, 2..3]);
    }

    #[test]
    fn pages_scroll_clamp_and_wrap() {
        let mut src = b"one\ntwo\nthree is longer\nfour\nfive\n".to_vec();
        let p = pager(UTF_8, 0, 4096);
        {
            let mut w = Walker::new(p, &mut src, None, None);
            let top = w.start();
            let page = w.page(top, 3).unwrap();
            assert_eq!(
                page.rows
                    .iter()
                    .map(|r| r.text.as_str())
                    .collect::<Vec<_>>(),
                ["one", "two", "three is longer"]
            );
            assert!(page.at_start && !page.at_end);
            let down = w.scroll(top, 10, 3).unwrap();
            assert_eq!(
                down,
                Position {
                    row_start: 8,
                    piece: 0
                },
                "clamped so the last page is full"
            );
            let page = w.page(down, 3).unwrap();
            assert_eq!(
                page.rows
                    .iter()
                    .map(|r| r.text.as_str())
                    .collect::<Vec<_>>(),
                ["three is longer", "four", "five"]
            );
            assert!(page.at_end);
            assert_eq!(w.end_top(3).unwrap(), down);
            assert_eq!(
                w.scroll(down, -1, 3).unwrap(),
                Position {
                    row_start: 4,
                    piece: 0
                }
            );
            assert_eq!(w.scroll(top, -5, 3).unwrap(), top);
            assert_eq!(
                w.position_at(17).unwrap(),
                Position {
                    row_start: 8,
                    piece: 0
                }
            );
            assert_eq!(
                w.position_at(26).unwrap(),
                Position {
                    row_start: 24,
                    piece: 0
                }
            );
        }
        // Wrapped at 5 columns, "three is longer" is 3 pieces.
        let mut w = Walker::new(p, &mut src, Some(5), None);
        let top = w.start();
        let page = w.page(top, 4).unwrap();
        assert_eq!(
            page.rows
                .iter()
                .map(|r| (r.text.as_str(), r.piece))
                .collect::<Vec<_>>(),
            [("one", 0), ("two", 0), ("three", 0), (" is l", 1)]
        );
        let two_down = w.advance(top, 3).unwrap();
        assert_eq!(
            two_down,
            Position {
                row_start: 8,
                piece: 1
            }
        );
        assert_eq!(
            w.retreat(two_down, 1).unwrap(),
            Position {
                row_start: 8,
                piece: 0
            }
        );
        assert_eq!(
            w.retreat(two_down, 2).unwrap(),
            Position {
                row_start: 4,
                piece: 0
            }
        );
        assert_eq!(
            w.position_at(8 + 12).unwrap(),
            Position {
                row_start: 8,
                piece: 2
            }
        );
    }

    #[test]
    fn highlights_land_on_the_right_characters_and_pieces() {
        let mut src = "ab\tcé-needle-x\n".as_bytes().to_vec();
        let p = pager(UTF_8, 0, 4096);
        let needle_at = src.windows(6).position(|w| w == b"needle").unwrap() as u64;
        {
            let mut w = Walker::new(p, &mut src, None, Some(needle_at..needle_at + 6));
            let page = w.page(w.start(), 1).unwrap();
            let row = &page.rows[0];
            let hl = row.highlight.clone().unwrap();
            assert_eq!(&row.text[hl], "needle");
        }
        // Wrapped at 8 columns the display text "ab      cé-needle-x"
        // splits into "ab      " / "cé-needl" / "e-x": the hit
        // straddles the second and third pieces.
        let mut w = Walker::new(p, &mut src, Some(8), Some(needle_at..needle_at + 6));
        let page = w.page(w.start(), 3).unwrap();
        let parts: Vec<String> = page
            .rows
            .iter()
            .filter_map(|r| r.highlight.clone().map(|h| r.text[h].to_string()))
            .collect();
        assert_eq!(parts, ["needl", "e"]);
    }
}
