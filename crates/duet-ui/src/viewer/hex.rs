// SPDX-License-Identifier: MIT
//! Hex and binary modes (T-5.3.6): fixed-width rows straight off the
//! byte offsets, so they are random access and need no index. Hex shows
//! `offset  XX XX ..  |ascii|`; binary (Total Commander's Ctrl+3) shows
//! 80 decoded characters per row with no hex column.

use std::ops::Range;

use super::source::ByteSource;

pub(crate) const HEX_WIDTHS: [usize; 4] = [8, 16, 24, 32];
pub(crate) const BINARY_WIDTH: usize = 80;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct HexLayout {
    pub(crate) width: usize,
    /// Offsets in decimal instead of hex.
    pub(crate) decimal: bool,
    /// Binary mode: characters only.
    pub(crate) chars_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HexRow {
    pub(crate) offset: u64,
    pub(crate) text: String,
    /// Byte range of `text` to highlight.
    pub(crate) highlight: Option<Range<usize>>,
}

pub(crate) fn row_count(len: u64, width: usize) -> u64 {
    len.div_ceil(width.max(1) as u64)
}

/// Width of the offset column for a file of `len` bytes.
pub(crate) fn offset_digits(len: u64, decimal: bool) -> usize {
    let last = len.saturating_sub(1);
    if decimal {
        (last.checked_ilog10().map_or(1, |d| d as usize + 1)).max(10)
    } else {
        (last.checked_ilog2().map_or(1, |b| b as usize / 4 + 1)).max(8)
    }
}

fn printable(b: u8) -> char {
    if (0x20..0x7F).contains(&b) {
        b as char
    } else if b >= 0xA0 {
        // Latin-1 range, as Total Commander shows high bytes.
        encoding_rs::WINDOWS_1252
            .decode_without_bom_handling(&[b])
            .0
            .chars()
            .next()
            .filter(|c| !c.is_control())
            .unwrap_or('.')
    } else {
        '.'
    }
}

pub(crate) fn format_row(
    offset: u64,
    bytes: &[u8],
    layout: HexLayout,
    digits: usize,
    highlight: Option<&Range<u64>>,
) -> HexRow {
    let width = layout.width.max(1);
    let mut text = if layout.decimal {
        format!("{offset:0digits$}  ")
    } else {
        format!("{offset:0digits$X}  ")
    };
    let hit = highlight.and_then(|hl| {
        let a = hl.start.max(offset);
        let b = hl.end.min(offset + bytes.len() as u64);
        (b > a).then(|| ((a - offset) as usize, (b - offset) as usize))
    });
    let mut hl_range = None;
    if layout.chars_only {
        let start = text.len();
        for &b in bytes {
            text.push(printable(b));
        }
        if let Some((a, b)) = hit {
            // One char per byte, all single UTF-8-encoded chars up to 2 bytes:
            // compute by char positions.
            let byte_at = |n: usize| {
                text[start..]
                    .char_indices()
                    .nth(n)
                    .map_or(text.len(), |(i, _)| start + i)
            };
            hl_range = Some(byte_at(a)..byte_at(b));
        }
        return HexRow {
            offset,
            text,
            highlight: hl_range,
        };
    }
    let hex_start = text.len();
    for i in 0..width {
        if i > 0 && i % 8 == 0 {
            text.push(' ');
        }
        match bytes.get(i) {
            Some(b) => {
                if let Some((a, _)) = hit
                    && i == a
                {
                    hl_range = Some(text.len()..text.len());
                }
                text.push_str(&format!("{b:02X}"));
                if let Some((_, b_end)) = hit
                    && i + 1 == b_end
                    && let Some(r) = hl_range.as_mut()
                {
                    r.end = text.len();
                }
            }
            None => text.push_str("  "),
        }
        text.push(' ');
    }
    debug_assert!(text.len() > hex_start);
    text.push_str(" |");
    for &b in bytes {
        text.push(printable(b));
    }
    for _ in bytes.len()..width {
        text.push(' ');
    }
    text.push('|');
    HexRow {
        offset,
        text,
        highlight: hl_range,
    }
}

pub(crate) fn rows<S: ByteSource>(
    src: &mut S,
    layout: HexLayout,
    first_row: u64,
    count: usize,
    highlight: Option<Range<u64>>,
) -> std::io::Result<Vec<HexRow>> {
    let len = src.len();
    let width = layout.width.max(1);
    let digits = offset_digits(len, layout.decimal);
    let start = first_row.saturating_mul(width as u64);
    if start >= len {
        return Ok(Vec::new());
    }
    let bytes = src.read(start, count.saturating_mul(width))?;
    Ok(bytes
        .chunks(width)
        .enumerate()
        .map(|(i, chunk)| {
            format_row(
                start + (i * width) as u64,
                chunk,
                layout,
                digits,
                highlight.as_ref(),
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_rows_are_fixed_width_with_an_ascii_gutter() {
        let mut src = b"Hello, world!\x00\xFF\xE9rest".to_vec();
        let layout = HexLayout {
            width: 16,
            decimal: false,
            chars_only: false,
        };
        let out = rows(&mut src, layout, 0, 10, Some(7..12)).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(row_count(src.len() as u64, 16), 2);
        let r = &out[0];
        assert!(
            r.text.starts_with(
                "00000000  48 65 6C 6C 6F 2C 20 77  6F 72 6C 64 21 00 FF E9  |Hello, world!.ÿé|"
            ),
            "{}",
            r.text
        );
        let hl = r.highlight.clone().unwrap();
        assert_eq!(
            &r.text[hl], "77  6F 72 6C 64",
            "bytes 7..12 highlighted across the group gap"
        );
        assert!(
            out[1].text.starts_with("00000010  72 65 73 74 "),
            "{}",
            out[1].text
        );
        assert!(out[1].text.ends_with("|rest            |"));
        assert!(out[1].highlight.is_none());
        assert!(rows(&mut src, layout, 2, 10, None).unwrap().is_empty());
    }

    #[test]
    fn decimal_offsets_binary_mode_and_digit_widths() {
        let mut src = b"abc\x01\xE9".to_vec();
        let layout = HexLayout {
            width: 80,
            decimal: true,
            chars_only: true,
        };
        let out = rows(&mut src, layout, 0, 1, Some(1..3)).unwrap();
        assert_eq!(out[0].text, "0000000000  abc.é");
        assert_eq!(&out[0].text[out[0].highlight.clone().unwrap()], "bc");
        assert_eq!(offset_digits(0x1_0000_0001, false), 9);
        assert_eq!(offset_digits(0x1_0000_0000, false), 8);
        assert_eq!(offset_digits(100, false), 8);
        assert_eq!(offset_digits(12_345_678_901, true), 11);
        assert_eq!(row_count(0, 16), 0);
        assert_eq!(row_count(17, 16), 2);
    }
}
