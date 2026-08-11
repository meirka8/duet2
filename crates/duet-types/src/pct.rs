//! Minimal percent-encoding, hand-rolled to keep `duet-types` at "no deps
//! beyond std/serde" (design.md §8.1).
//!
//! `VPath`'s grammar uses `:`, `/`, `!`, `@` as structural separators. Any
//! occurrence of those bytes (plus `%`, the escape character itself) inside
//! *data* — a path segment, a host, a user — is percent-encoded so that
//! parsing can find the real separators unambiguously (see `path.rs` for
//! why this matters for nested `VPath`s specifically).

use crate::path::PathParseError;

/// Bytes that never need encoding anywhere we use this: unreserved per
/// RFC 3986 (`ALPHA / DIGIT / "-" / "." / "_" / "~"`).
fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

/// Additional bytes considered safe to leave literal within a single path
/// *segment* (i.e. between `/`s). Excludes `!` (nesting separator) and `%`
/// (escape char); `/` never appears within a single segment by definition.
fn is_path_segment_safe(b: u8) -> bool {
    is_unreserved(b)
        || matches!(
            b,
            b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=' | b':' | b'@'
        )
}

/// Additional bytes considered safe within an authority `user`/`host`
/// field. Excludes `:`, `@`, `!`, `%`, `/`.
fn is_authority_safe(b: u8) -> bool {
    is_unreserved(b)
        || matches!(
            b,
            b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'='
        )
}

fn encode(s: &str, safe: fn(u8) -> bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if safe(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Percent-encodes a single path segment (a filename).
pub(crate) fn encode_path_segment(s: &str) -> String {
    encode(s, is_path_segment_safe)
}

/// Percent-encodes an authority `user` or `host` field.
pub(crate) fn encode_authority(s: &str) -> String {
    encode(s, is_authority_safe)
}

/// Decodes `%XX` escapes, validating the result is well-formed UTF-8.
pub(crate) fn decode(s: &str) -> Result<String, PathParseError> {
    if !s.as_bytes().contains(&b'%') {
        // Fast path: nothing to decode.
        return Ok(s.to_string());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .ok_or(PathParseError::InvalidPercentEncoding)?;
            let hi = (hex[0] as char)
                .to_digit(16)
                .ok_or(PathParseError::InvalidPercentEncoding)?;
            let lo = (hex[1] as char)
                .to_digit(16)
                .ok_or(PathParseError::InvalidPercentEncoding)?;
            out.push(((hi << 4) | lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| PathParseError::InvalidPercentEncoding)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_reserved_chars() {
        let s = "weird!name%with:stuff@x";
        let encoded = encode_path_segment(s);
        assert!(!encoded.contains('!'));
        assert!(!encoded.contains('%') || encoded.contains("%21") || encoded.contains("%25"));
        assert_eq!(decode(&encoded).unwrap(), s);
    }

    #[test]
    fn plain_ascii_is_untouched() {
        assert_eq!(
            encode_path_segment("hello-world_1.2.txt"),
            "hello-world_1.2.txt"
        );
    }

    #[test]
    fn decode_rejects_truncated_escape() {
        assert!(decode("abc%2").is_err());
    }

    #[test]
    fn decode_rejects_invalid_hex() {
        assert!(decode("abc%zz").is_err());
    }
}
