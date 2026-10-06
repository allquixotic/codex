//! Case-insensitive search that reports UTF-8 byte ranges in the original
//! text, as `TextInput.set-selection-offsets` expects.

use std::ops::Range;

/// Upper bound on reported matches; the find bar shows "N+" beyond it.
pub(crate) const MAX_MATCHES: usize = 10_000;
/// Matches further than this into their line are not highlighted (the
/// view measures the text before a match to place its highlight).
const MAX_MARK_PREFIX_BYTES: usize = 4096;

/// Highlight of one match in the unwrapped text view: the view measures
/// `prefix` (the line up to the match) and `text` in the content font to
/// place it exactly, whatever the characters' widths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Mark {
    /// Zero-based line of the match.
    pub(crate) line: usize,
    pub(crate) prefix: String,
    pub(crate) text: String,
    pub(crate) current: bool,
}

/// Highlights for the matches that start on `lines` (zero-based, end
/// exclusive), at most `limit`. A match that runs past the end of its line
/// is highlighted up to the line end.
pub(crate) fn marks_in_lines(
    text: &str,
    line_starts: &[usize],
    matches: &[Range<usize>],
    current: Option<usize>,
    lines: Range<usize>,
    limit: usize,
) -> Vec<Mark> {
    let Some(&from) = line_starts.get(lines.start) else {
        return Vec::new();
    };
    let to = line_starts
        .get(lines.end)
        .copied()
        .unwrap_or(text.len() + 1);
    let first = matches.partition_point(|range| range.start < from);
    let mut marks = Vec::new();
    for (index, range) in matches.iter().enumerate().skip(first) {
        if range.start >= to || marks.len() == limit {
            break;
        }
        let line = line_starts
            .partition_point(|&start| start <= range.start)
            .saturating_sub(1);
        let line_start = line_starts[line];
        let Some(prefix) = text.get(line_start..range.start) else {
            continue;
        };
        if prefix.len() > MAX_MARK_PREFIX_BYTES {
            continue;
        }
        let Some(matched) = text.get(range.clone()) else {
            continue;
        };
        let matched = matched.split('\n').next().unwrap_or_default();
        if matched.is_empty() {
            continue;
        }
        marks.push(Mark {
            line,
            prefix: prefix.to_string(),
            text: matched.to_string(),
            current: current == Some(index),
        });
    }
    marks
}

/// Matches of one query.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FindResult {
    pub(crate) matches: Vec<Range<usize>>,
    /// More matches exist than were collected.
    pub(crate) truncated: bool,
}

/// Bytes folded or scanned between checks for cancellation.
const CANCEL_CHECK_BYTES: usize = 1 << 20;

/// Finds non-overlapping, case-insensitive occurrences of `needle`.
///
/// Both strings are folded one char at a time (so char boundaries line up
/// between the folded and original text), searched, and the match positions
/// mapped back to byte offsets in `haystack`.
#[cfg(test)]
pub(crate) fn find_all(haystack: &str, needle: &str, limit: usize) -> FindResult {
    let never = || false;
    fold_cancellable(haystack, &never)
        .and_then(|folded| find_in(&folded, needle, limit, &never))
        .unwrap_or_default()
}

/// Like [`find_all`] over an already folded haystack (folding a large
/// file is most of the work, and the text does not change while a query is
/// typed). `None` when `cancelled` returned true.
pub(crate) fn find_in(
    folded: &Folded,
    needle: &str,
    limit: usize,
    cancelled: &dyn Fn() -> bool,
) -> Option<FindResult> {
    let needle = fold(needle).text;
    let mut result = FindResult::default();
    if needle.is_empty() || limit == 0 {
        return Some(result);
    }
    let text = folded.text.as_str();
    let mut from = 0;
    // The text is scanned in windows so a search that is no longer wanted
    // stops early; a match may run past the end of its window.
    while from < text.len() {
        if cancelled() {
            return None;
        }
        let window_end = char_boundary_at_or_after(text, from + CANCEL_CHECK_BYTES);
        let search_end = char_boundary_at_or_after(text, window_end + needle.len());
        let mut next = window_end;
        for (offset, matched) in text[from..search_end].match_indices(&needle) {
            let start = from + offset;
            if start >= window_end {
                break;
            }
            if result.matches.len() == limit {
                result.truncated = true;
                return Some(result);
            }
            let end = start + matched.len();
            result
                .matches
                .push(folded.original_offset(start)..folded.original_offset(end));
            next = next.max(end);
        }
        from = next;
    }
    Some(result)
}

fn char_boundary_at_or_after(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Index of the first match that starts at or after `offset`, wrapping to
/// the first match.
pub(crate) fn match_at_or_after(matches: &[Range<usize>], offset: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    let index = matches.partition_point(|range| range.start < offset);
    Some(if index == matches.len() { 0 } else { index })
}

fn fold_char(ch: char) -> char {
    // Multi-char lowercase expansions (only `İ`) keep their first char so
    // the folded text has exactly one char per original char.
    ch.to_lowercase().next().unwrap_or(ch)
}

/// Text folded for case-insensitive search, with the map back to offsets
/// in the original text.
pub(crate) struct Folded {
    text: String,
    /// `(folded, original)` offset pairs recorded after every char whose
    /// folded form has a different UTF-8 length. Between checkpoints the two
    /// offsets advance together.
    checkpoints: Vec<(usize, usize)>,
}

impl Folded {
    /// Maps a char-boundary offset in the folded text to the original text.
    fn original_offset(&self, folded: usize) -> usize {
        let index = self
            .checkpoints
            .partition_point(|&(checkpoint, _)| checkpoint <= folded);
        match index.checked_sub(1).map(|index| self.checkpoints[index]) {
            Some((checkpoint, original)) => original + (folded - checkpoint),
            None => folded,
        }
    }
}

fn fold(text: &str) -> Folded {
    fold_cancellable(text, &|| false).unwrap_or(Folded {
        text: String::new(),
        checkpoints: Vec::new(),
    })
}

/// Folds `text`; `None` when `cancelled` returned true.
pub(crate) fn fold_cancellable(text: &str, cancelled: &dyn Fn() -> bool) -> Option<Folded> {
    let mut folded = String::with_capacity(text.len());
    let mut checkpoints = Vec::new();
    let mut next_check = CANCEL_CHECK_BYTES;
    for (offset, ch) in text.char_indices() {
        if offset >= next_check {
            if cancelled() {
                return None;
            }
            next_check = offset + CANCEL_CHECK_BYTES;
        }
        let lower = fold_char(ch);
        folded.push(lower);
        if lower.len_utf8() != ch.len_utf8() {
            checkpoints.push((folded.len(), offset + ch.len_utf8()));
        }
    }
    Some(Folded {
        text: folded,
        checkpoints,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn ranges(haystack: &str, needle: &str) -> Vec<Range<usize>> {
        find_all(haystack, needle, MAX_MATCHES).matches
    }

    #[test]
    fn ascii_case_insensitive() {
        assert_eq!(ranges("Foo foo FOO", "foo"), vec![0..3, 4..7, 8..11]);
        assert_eq!(ranges("aaaa", "aa"), vec![0..2, 2..4]);
        assert_eq!(ranges("abc", ""), Vec::<Range<usize>>::new());
        assert_eq!(ranges("abc", "x"), Vec::<Range<usize>>::new());
    }

    #[test]
    fn multibyte_offsets_are_original_bytes() {
        let text = "naïve — NAÏVE ✓ naïve";
        let found = ranges(text, "NAÏVE");
        assert_eq!(found.len(), 3);
        for range in &found {
            assert_eq!(text[range.clone()].to_lowercase(), "naïve");
        }
        assert_eq!(found[0], 0..6);
    }

    #[test]
    fn length_changing_folds_map_back() {
        // U+212A KELVIN SIGN (3 bytes) lowercases to ASCII 'k' (1 byte), and
        // U+023A (2 bytes) lowercases to U+2C65 (3 bytes).
        let text = "\u{212a}elvin x \u{23a}b kelvin";
        let found = ranges(text, "kelvin");
        assert_eq!(found, vec![0..8, 15..21]);
        assert_eq!(&text[15..21], "kelvin");
        let found = ranges(text, "\u{2c65}B");
        assert_eq!(found, vec![11..14]);
        assert_eq!(&text[11..14], "\u{23a}b");
    }

    #[test]
    fn dotted_capital_i_matches_i() {
        let text = "İstanbul istanbul";
        let found = ranges(text, "istanbul");
        assert_eq!(found, vec![0..9, 10..18]);
    }

    #[test]
    fn limit_truncates() {
        let result = find_all("a a a a", "a", 2);
        assert_eq!(result.matches, vec![0..1, 2..3]);
        assert!(result.truncated);
    }

    #[test]
    fn matches_across_scan_windows_are_found_once() {
        // Matches that straddle a window boundary, and a non-ASCII char at
        // the boundary.
        let mut text = "x".repeat(CANCEL_CHECK_BYTES - 2);
        text.push_str("needle é needle");
        text.push_str(&"y".repeat(CANCEL_CHECK_BYTES));
        text.push_str("NEEDLE");
        let found = ranges(&text, "needle");
        let expected = vec![
            CANCEL_CHECK_BYTES - 2..CANCEL_CHECK_BYTES + 4,
            CANCEL_CHECK_BYTES + 8..CANCEL_CHECK_BYTES + 14,
            text.len() - 6..text.len(),
        ];
        assert_eq!(found, expected);
        // Overlapping candidates stay non-overlapping across windows.
        let aaa = "a".repeat(CANCEL_CHECK_BYTES + 3);
        let found = find_all(&aaa, "aa", usize::MAX).matches;
        assert_eq!(found.len(), (CANCEL_CHECK_BYTES + 3) / 2);
        assert!(found.windows(2).all(|pair| pair[0].end <= pair[1].start));
    }

    #[test]
    fn searches_stop_when_cancelled() {
        let text = "abc ".repeat(CANCEL_CHECK_BYTES);
        assert!(fold_cancellable(&text, &|| true).is_none());
        let folded = fold_cancellable(&text, &|| false);
        let result = folded
            .as_ref()
            .and_then(|folded| find_in(folded, "ABC", MAX_MATCHES, &|| true));
        assert!(result.is_none());
        // The folded text is reused for the next query.
        let result = folded.and_then(|folded| find_in(&folded, "bc a", MAX_MATCHES, &|| false));
        assert_eq!(result.map(|result| result.matches.len()), Some(MAX_MATCHES));
    }

    #[test]
    fn marks_cover_requested_lines() {
        let text = "alpha beta\n\tbeta gamma\nbeta\n";
        let starts = vec![0, 11, 23, 28];
        let matches = ranges(text, "beta");
        assert_eq!(matches, vec![6..10, 12..16, 23..27]);
        let marks = marks_in_lines(text, &starts, &matches, Some(1), 1..3, 10);
        assert_eq!(
            marks,
            vec![
                Mark {
                    line: 1,
                    prefix: "\t".to_string(),
                    text: "beta".to_string(),
                    current: true,
                },
                Mark {
                    line: 2,
                    prefix: String::new(),
                    text: "beta".to_string(),
                    current: false,
                },
            ]
        );
        assert_eq!(
            marks_in_lines(text, &starts, &matches, None, 0..1, 10)
                .iter()
                .map(|mark| (mark.line, mark.prefix.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "alpha ")]
        );
        // The limit and lines past the end.
        assert_eq!(
            marks_in_lines(text, &starts, &matches, None, 0..4, 2).len(),
            2
        );
        assert!(marks_in_lines(text, &starts, &matches, None, 9..12, 10).is_empty());
    }

    #[test]
    fn next_match_wraps() {
        let matches = vec![2..3, 5..6, 9..10];
        assert_eq!(match_at_or_after(&matches, 0), Some(0));
        assert_eq!(match_at_or_after(&matches, 3), Some(1));
        assert_eq!(match_at_or_after(&matches, 5), Some(1));
        assert_eq!(match_at_or_after(&matches, 10), Some(0));
        assert_eq!(match_at_or_after(&[], 0), None);
    }
}
