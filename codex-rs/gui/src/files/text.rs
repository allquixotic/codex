//! Pure helpers for showing file bytes as text: chunk boundaries, binary
//! sniffing, decoding, and the line index used for the gutter, "go to line",
//! and cursor positions.

use std::ops::Range;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// Bytes shown per chunk ("first 2 MiB", then "Load more").
pub(crate) const CHUNK_BYTES: u64 = 2 * 1024 * 1024;
/// Never keep more than this much of one file in the viewer.
pub(crate) const MAX_LOADED_BYTES: u64 = 64 * 1024 * 1024;
/// Prefix inspected to decide whether a file is binary.
const SNIFF_BYTES: usize = 8 * 1024;

/// Whether `head` (the first bytes of a file) looks like binary data.
///
/// Mirrors git's heuristic (a NUL byte means binary) and also rejects
/// content dominated by control characters.
pub(crate) fn looks_binary(head: &[u8]) -> bool {
    let sample = &head[..head.len().min(SNIFF_BYTES)];
    if sample.contains(&0) {
        return true;
    }
    if sample.is_empty() {
        return false;
    }
    let control = sample
        .iter()
        .filter(|&&byte| byte < 0x20 && !matches!(byte, b'\t' | b'\n' | b'\r' | 0x0c | 0x1b | 0x08))
        .count();
    control * 10 > sample.len()
}

/// Number of leading bytes of `buf` to decode now.
///
/// `eof` means `buf` reaches the end of the file, so everything is used.
/// Otherwise the cut lands after the last newline when that keeps at least
/// half of the buffer (so lines are not split between chunks), and else on a
/// UTF-8 character boundary that does not separate `\r` from `\n`. The bytes
/// after the cut are read again with the next chunk.
pub(crate) fn chunk_cut(buf: &[u8], eof: bool) -> usize {
    if eof || buf.is_empty() {
        return buf.len();
    }
    if let Some(newline) = buf.iter().rposition(|&byte| byte == b'\n')
        && (newline + 1) * 2 >= buf.len()
    {
        return newline + 1;
    }
    let mut cut = utf8_boundary(buf);
    if cut > 0 && buf[cut - 1] == b'\r' {
        cut -= 1;
    }
    if cut == 0 { buf.len() } else { cut }
}

/// Largest prefix length of `buf` that does not end inside a UTF-8 sequence.
fn utf8_boundary(buf: &[u8]) -> usize {
    let len = buf.len();
    // A sequence is at most 4 bytes, so its lead byte is among the last 4.
    for back in 1..=len.min(4) {
        let index = len - back;
        let byte = buf[index];
        if byte & 0xc0 == 0x80 {
            continue;
        }
        let width = match byte {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        return if index + width <= len { len } else { index };
    }
    // No lead byte: invalid data that lossy decoding replaces anyway.
    len
}

/// Decodes file bytes for display: lossy UTF-8, a leading BOM removed when
/// `at_file_start`, and CRLF / lone CR line endings turned into LF so the
/// line index matches what the text widget renders.
pub(crate) fn decode_text(bytes: &[u8], at_file_start: bool) -> String {
    let bytes = if at_file_start {
        bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes)
    } else {
        bytes
    };
    let text = String::from_utf8_lossy(bytes);
    if !text.contains('\r') {
        return text.into_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Byte offsets at which each visual line of `text` starts. Always begins
/// with 0; a trailing newline yields a final (empty) line, like the text
/// widget renders it.
pub(crate) fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(
            text.bytes()
                .enumerate()
                .filter(|&(_, byte)| byte == b'\n')
                .map(|(index, _)| index + 1),
        )
        .collect()
}

/// Number of lines that get a number in the gutter: the trailing empty line
/// after a final newline does not count.
pub(crate) fn numbered_lines(text: &str, starts: &[usize]) -> usize {
    if text.is_empty() {
        0
    } else if text.ends_with('\n') {
        starts.len().saturating_sub(1)
    } else {
        starts.len()
    }
}

/// Gutter text: the numbers `1..=count`, one per line.
///
/// The gutter is laid out by the same text engine as the content, so line
/// positions match exactly even where accumulated rounding would make a
/// computed `line * height` drift on very long files.
pub(crate) fn gutter_numbers(count: usize) -> String {
    let digits = count.max(1).to_string().len();
    let mut out = String::with_capacity(count * (digits + 1));
    for line in 1..=count {
        if line > 1 {
            out.push('\n');
        }
        out.push_str(&line.to_string());
    }
    out
}

/// Zero-based line containing byte `offset`.
pub(crate) fn line_of_offset(starts: &[usize], offset: usize) -> usize {
    starts
        .partition_point(|&start| start <= offset)
        .saturating_sub(1)
}

/// One-based `(line, column)` of byte `offset`, counting columns in chars.
pub(crate) fn line_col(text: &str, starts: &[usize], offset: usize) -> (usize, usize) {
    let offset = floor_char_boundary(text, offset.min(text.len()));
    let line = line_of_offset(starts, offset);
    let start = starts.get(line).copied().unwrap_or(0).min(offset);
    let column = text[start..offset].chars().count() + 1;
    (line + 1, column)
}

/// Byte range of one-based `line` without its newline, or `None` when the
/// line is not in `text`.
pub(crate) fn line_range(text: &str, starts: &[usize], line: usize) -> Option<Range<usize>> {
    let index = line.checked_sub(1)?;
    let start = *starts.get(index)?;
    let end = match starts.get(index + 1) {
        Some(next) => next - 1,
        None => text.len(),
    };
    Some(start..end.max(start))
}

fn floor_char_boundary(text: &str, mut offset: usize) -> usize {
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// Human-readable byte count ("512 B", "12.3 KiB", "2.0 MiB").
pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// A file name (without extension) suggested for saving text titled
/// `title`: characters that are not allowed in file names on any platform
/// become `-`, and long titles are cut.
pub(crate) fn file_stem_for(title: &str) -> String {
    const MAX_CHARS: usize = 80;
    let cleaned: String = title
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            ch if ch.is_control() => ' ',
            ch => ch,
        })
        .collect();
    let stem: String = cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_CHARS)
        .collect();
    let stem = stem.trim_matches(|ch: char| ch == '.' || ch == ' ' || ch == '-');
    if stem.is_empty() {
        "text".to_string()
    } else {
        stem.to_string()
    }
}

/// Removes `.` and resolves `..` components without touching the disk.
pub(crate) fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn binary_detection() {
        assert!(!looks_binary(b""));
        assert!(!looks_binary(b"fn main() {\n\tprintln!(\"hi\");\r\n}\n"));
        assert!(!looks_binary("héllo wörld ✓".as_bytes()));
        assert!(looks_binary(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"));
        assert!(looks_binary(&[0x01, 0x02, 0x03, 0x04, b'a', b'b']));
        // A NUL past the sniffed prefix does not count.
        let mut late = vec![b'a'; SNIFF_BYTES];
        late.push(0);
        assert!(!looks_binary(&late));
    }

    #[test]
    fn chunk_cut_prefers_last_newline() {
        assert_eq!(chunk_cut(b"one\ntwo\nthr", /*eof*/ false), 8);
        assert_eq!(chunk_cut(b"one\ntwo\nthr", /*eof*/ true), 11);
        assert_eq!(chunk_cut(b"", /*eof*/ false), 0);
    }

    #[test]
    fn chunk_cut_falls_back_to_char_boundary() {
        // Newline too early: keep the long line but never split "é" (c3 a9).
        let mut buf = b"x\n".to_vec();
        buf.extend_from_slice(&[b'a'; 10]);
        buf.extend_from_slice(&[0xc3]);
        assert_eq!(chunk_cut(&buf, /*eof*/ false), 12);
        // A complete multibyte char at the end is kept.
        buf.push(0xa9);
        assert_eq!(chunk_cut(&buf, /*eof*/ false), 14);
        // Four-byte emoji cut after two bytes.
        let mut emoji = vec![b'a'; 6];
        emoji.extend_from_slice(&"😀".as_bytes()[..2]);
        assert_eq!(chunk_cut(&emoji, /*eof*/ false), 6);
        // Three-byte char cut after its lead byte.
        let mut check = vec![b'a'; 6];
        check.extend_from_slice(&"✓".as_bytes()[..1]);
        assert_eq!(chunk_cut(&check, /*eof*/ false), 6);
    }

    #[test]
    fn chunk_cut_keeps_crlf_together() {
        let mut buf = b"a\n".to_vec();
        buf.extend_from_slice(&[b'b'; 10]);
        buf.push(b'\r');
        assert_eq!(chunk_cut(&buf, /*eof*/ false), 12);
    }

    #[test]
    fn chunks_reassemble_multibyte_text() {
        let text = "ab€cd😀ef\nghé";
        let bytes = text.as_bytes();
        for size in 1..bytes.len() {
            let mut decoded = String::new();
            let mut offset = 0;
            while offset < bytes.len() {
                let end = (offset + size).min(bytes.len());
                let window = &bytes[offset..end];
                let cut = chunk_cut(window, /*eof*/ end == bytes.len());
                decoded.push_str(&decode_text(&window[..cut], offset == 0));
                offset += cut;
            }
            if size >= 4 {
                assert_eq!(decoded, text, "chunk size {size}");
            }
        }
    }

    #[test]
    fn decode_normalizes_line_endings_and_bom() {
        assert_eq!(
            decode_text(b"\xef\xbb\xbfa\r\nb\rc\n", /*at_file_start*/ true),
            "a\nb\nc\n"
        );
        assert_eq!(
            decode_text(b"\xef\xbb\xbfa", /*at_file_start*/ false),
            "\u{feff}a"
        );
        assert_eq!(
            decode_text(b"ok\xffok", /*at_file_start*/ true),
            "ok\u{fffd}ok"
        );
    }

    #[test]
    fn line_index_and_numbering() {
        let text = "one\ntwo\n";
        let starts = line_starts(text);
        assert_eq!(starts, vec![0, 4, 8]);
        assert_eq!(numbered_lines(text, &starts), 2);
        let partial = "one\ntwo";
        assert_eq!(numbered_lines(partial, &line_starts(partial)), 2);
        assert_eq!(numbered_lines("", &line_starts("")), 0);
        assert_eq!(line_of_offset(&starts, 0), 0);
        assert_eq!(line_of_offset(&starts, 3), 0);
        assert_eq!(line_of_offset(&starts, 4), 1);
        assert_eq!(line_of_offset(&starts, 99), 2);
    }

    #[test]
    fn gutter_lists_line_numbers() {
        assert_eq!(gutter_numbers(0), "");
        assert_eq!(gutter_numbers(3), "1\n2\n3");
        assert_eq!(gutter_numbers(12).lines().count(), 12);
    }

    #[test]
    fn line_col_counts_chars() {
        let text = "héllo\nwörld";
        let starts = line_starts(text);
        assert_eq!(line_col(text, &starts, 0), (1, 1));
        // Byte 3 is after "hé" (h=1 byte, é=2 bytes).
        assert_eq!(line_col(text, &starts, 3), (1, 3));
        // Inside a multibyte char: snapped back to its start.
        assert_eq!(line_col(text, &starts, 2), (1, 2));
        assert_eq!(line_col(text, &starts, 7), (2, 1));
        assert_eq!(line_col(text, &starts, text.len()), (2, 6));
    }

    #[test]
    fn line_ranges() {
        let text = "a\nbc\n";
        let starts = line_starts(text);
        assert_eq!(line_range(text, &starts, 1), Some(0..1));
        assert_eq!(line_range(text, &starts, 2), Some(2..4));
        assert_eq!(line_range(text, &starts, 3), Some(5..5));
        assert_eq!(line_range(text, &starts, 4), None);
        assert_eq!(line_range(text, &starts, 0), None);
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(2 * 1024 * 1024), "2.0 MiB");
    }

    #[test]
    fn file_stems() {
        assert_eq!(file_stem_for("Reply · repo: a/b"), "Reply · repo- a-b");
        assert_eq!(file_stem_for("  ..\n "), "text");
        assert_eq!(file_stem_for(&"x".repeat(100)).len(), 80);
    }

    #[test]
    fn lexical_normalization() {
        assert_eq!(
            normalize_lexically(Path::new("/a/./b/../c/d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(
            normalize_lexically(Path::new("/a/b/")),
            PathBuf::from("/a/b")
        );
    }
}
