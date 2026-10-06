//! Pure text helpers for the composer: the sigil token under the cursor,
//! completion insertion, path quoting, `$skill` extraction and the Enter key
//! decision. Offsets are UTF-8 byte offsets, matching Slint's `TextInput`.

use std::ops::Range;

/// A sigil-prefixed token (`@path`, `$skill`) under the cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Token {
    /// Byte range of the whole token, sigil included.
    pub(crate) range: Range<usize>,
    /// Token text after the sigil (may be empty).
    pub(crate) query: String,
}

/// What Enter does in the composer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnterAction {
    /// Submit the message.
    Send,
    /// Let the text input insert a line break.
    Newline,
}

/// Decides what Enter does given the user's preference and the modifiers.
///
/// `command` is Slint's `control` modifier, which is Cmd on macOS and Ctrl
/// elsewhere; `meta` is the physical Ctrl key on macOS. With `enter_sends`,
/// Shift or Alt inserts a newline. Without it, Ctrl/Cmd+Enter sends.
pub(crate) fn enter_action(
    enter_sends: bool,
    shift: bool,
    command: bool,
    meta: bool,
    alt: bool,
) -> EnterAction {
    if enter_sends {
        if (shift || alt) && !command && !meta {
            EnterAction::Newline
        } else {
            EnterAction::Send
        }
    } else if command || meta {
        EnterAction::Send
    } else {
        EnterAction::Newline
    }
}

/// Largest char boundary of `text` at or before `offset`.
pub(crate) fn floor_char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// The whitespace-delimited token containing the cursor, when it starts with
/// `sigil`.
///
/// A cursor directly after the token (before whitespace or the end) still
/// counts as inside it, so the popup stays open while typing. A cursor after
/// whitespace belongs to the token that starts there, if any.
pub(crate) fn token_at_cursor(text: &str, cursor: usize, sigil: char) -> Option<Token> {
    let cursor = floor_char_boundary(text, cursor);
    let before = &text[..cursor];
    let after = &text[cursor..];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(index, c)| index + c.len_utf8());
    let end = cursor
        + after
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map_or(after.len(), |(index, _)| index);
    if start == end {
        return None;
    }
    let token = &text[start..end];
    let query = token.strip_prefix(sigil)?;
    Some(Token {
        range: start..end,
        query: query.to_string(),
    })
}

/// The slash command being typed: the first token of the message while the
/// cursor is inside it. Returns the name typed so far (without `/`).
pub(crate) fn slash_query(text: &str, cursor: usize) -> Option<String> {
    let rest = text.strip_prefix('/')?;
    let name_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let token_end = 1 + name_len;
    if cursor > token_end {
        return None;
    }
    Some(rest[..name_len].to_string())
}

/// Quotes a path for insertion into the message when it contains whitespace
/// (same rule as the TUI: no quoting when the path already has a `"`).
pub(crate) fn quote_path(path: &str) -> String {
    if path.chars().any(char::is_whitespace) && !path.contains('"') {
        format!("\"{path}\"")
    } else {
        path.to_string()
    }
}

/// Replaces `range` with `inserted` and leaves exactly one separating space
/// after it. Returns the new text and the cursor position after the space.
pub(crate) fn insert_completion(
    text: &str,
    range: Range<usize>,
    inserted: &str,
) -> (String, usize) {
    let start = floor_char_boundary(text, range.start);
    let end = floor_char_boundary(text, range.end.max(start));
    let suffix = &text[end..];
    let mut out = String::with_capacity(text.len() + inserted.len() + 1);
    out.push_str(&text[..start]);
    out.push_str(inserted);
    let cursor = match suffix.chars().next() {
        Some(' ' | '\t') => out.len() + 1,
        _ => {
            if !inserted.is_empty() {
                out.push(' ');
            }
            out.len()
        }
    };
    out.push_str(suffix);
    (out, cursor)
}

/// Removes `range` (for example an `@image.png` token that became an
/// attachment) together with one following space. Returns the new text and
/// the cursor position where the token was.
pub(crate) fn remove_range(text: &str, range: Range<usize>) -> (String, usize) {
    let start = floor_char_boundary(text, range.start);
    let mut end = floor_char_boundary(text, range.end.max(start));
    if text[end..].starts_with(' ') {
        end += 1;
    }
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(&text[end..]);
    (out, start)
}

/// Inserts `inserted` at `cursor` and returns the new text and the cursor
/// position after the insertion.
pub(crate) fn insert_at(text: &str, cursor: usize, inserted: &str) -> (String, usize) {
    let cursor = floor_char_boundary(text, cursor);
    let mut out = String::with_capacity(text.len() + inserted.len());
    out.push_str(&text[..cursor]);
    out.push_str(inserted);
    let new_cursor = out.len();
    out.push_str(&text[cursor..]);
    (out, new_cursor)
}

fn is_mention_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':')
}

/// Names written as `$name` in plain text, in order, deduplicated.
///
/// A mention starts at the beginning of the text or after a character that
/// cannot be part of a name, so `a$b` and `$$` are ignored. Trailing `:` is
/// treated as punctuation (`$review:` mentions `review`).
pub(crate) fn dollar_mentions(text: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut previous: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        let starts = c == '$' && previous.is_none_or(|p| !is_mention_name_char(p) && p != '$');
        previous = Some(c);
        if !starts {
            continue;
        }
        let name_start = index + 1;
        let mut name_end = name_start;
        while let Some(&(next_index, next)) = chars.peek() {
            if !is_mention_name_char(next) {
                break;
            }
            name_end = next_index + next.len_utf8();
            previous = Some(next);
            chars.next();
        }
        let name = text[name_start..name_end].trim_end_matches(':');
        if !name.is_empty() && !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// Case-insensitive match of `query` in `candidate`: rank (0 = prefix,
/// 1 = substring, 2 = subsequence) and the matched char indices.
pub(crate) fn fuzzy_score(candidate: &str, query: &str) -> Option<(u8, Vec<u32>)> {
    let candidate: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();
    let query: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let span = |start: usize| -> Vec<u32> {
        (start..start + query.len())
            .filter_map(|index| u32::try_from(index).ok())
            .collect()
    };
    if candidate.starts_with(&query) {
        return Some((0, span(0)));
    }
    if let Some(start) = candidate
        .windows(query.len())
        .position(|window| window == query.as_slice())
    {
        return Some((1, span(start)));
    }
    let mut indices = Vec::with_capacity(query.len());
    let mut wanted = query.iter().peekable();
    for (index, c) in candidate.iter().enumerate() {
        if wanted.peek() == Some(&c) {
            wanted.next();
            indices.push(u32::try_from(index).ok()?);
        }
    }
    wanted.peek().is_none().then_some((2, indices))
}

/// Escapes text for `StyledText::from_markdown` so it renders literally.
pub(crate) fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\n' | '\r' | '\t' => out.push(' '),
            c if c.is_ascii_punctuation() => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// Markdown for `text` with the characters at `indices` (char indices, as
/// reported by the fuzzy matcher) colored with `highlight` (`#rrggbb`).
pub(crate) fn highlighted_markdown(text: &str, indices: &[u32], highlight: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    let mut open = false;
    for (index, c) in text.chars().enumerate() {
        let matched = u32::try_from(index).is_ok_and(|index| indices.binary_search(&index).is_ok());
        if matched && !open {
            out.push_str(&format!("<font color=\"{highlight}\">"));
            open = true;
        } else if !matched && open {
            out.push_str("</font>");
            open = false;
        }
        out.push_str(&escape_markdown(&c.to_string()));
    }
    if open {
        out.push_str("</font>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn token(text: &str, cursor: usize) -> Option<(String, Range<usize>)> {
        token_at_cursor(text, cursor, '@').map(|token| (token.query, token.range))
    }

    #[test]
    fn enter_sends_by_default_and_shift_adds_a_line() {
        assert_eq!(
            enter_action(true, false, false, false, false),
            EnterAction::Send
        );
        assert_eq!(
            enter_action(true, true, false, false, false),
            EnterAction::Newline
        );
        assert_eq!(
            enter_action(true, false, false, false, true),
            EnterAction::Newline
        );
        assert_eq!(
            enter_action(true, false, true, false, false),
            EnterAction::Send
        );
        assert_eq!(
            enter_action(true, true, true, false, false),
            EnterAction::Send
        );
    }

    #[test]
    fn ctrl_enter_sends_when_enter_inserts_lines() {
        assert_eq!(
            enter_action(false, false, false, false, false),
            EnterAction::Newline
        );
        assert_eq!(
            enter_action(false, true, false, false, false),
            EnterAction::Newline
        );
        assert_eq!(
            enter_action(false, false, true, false, false),
            EnterAction::Send
        );
        assert_eq!(
            enter_action(false, false, false, true, false),
            EnterAction::Send
        );
    }

    #[test]
    fn finds_at_token_around_cursor() {
        assert_eq!(token("@src", 4), Some(("src".to_string(), 0..4)));
        assert_eq!(
            token("see @src/ma", 11),
            Some(("src/ma".to_string(), 4..11))
        );
        assert_eq!(
            token("see @src/ma now", 8),
            Some(("src/ma".to_string(), 4..11))
        );
        assert_eq!(token("see @", 5), Some((String::new(), 4..5)));
        // Cursor right after whitespace starts the next token.
        assert_eq!(token("a @b", 2), Some(("b".to_string(), 2..4)));
    }

    #[test]
    fn ignores_non_mentions() {
        assert_eq!(token("mail me@x.com", 13), None);
        assert_eq!(token("@src done", 9), None);
        assert_eq!(token("@src ", 5), None);
        assert_eq!(token("", 0), None);
        assert_eq!(token("plain", 3), None);
    }

    #[test]
    fn handles_multibyte_text_and_bad_offsets() {
        let text = "héllo @wörld";
        assert_eq!(
            token(text, text.len()),
            Some(("wörld".to_string(), 7..text.len()))
        );
        // An offset inside a multi-byte character snaps back to a boundary.
        assert_eq!(token("é@x", 1), None);
        assert_eq!(token("@x", 99), Some(("x".to_string(), 0..2)));
    }

    #[test]
    fn slash_query_only_inside_first_token() {
        assert_eq!(slash_query("/mo", 3), Some("mo".to_string()));
        assert_eq!(slash_query("/", 1), Some(String::new()));
        assert_eq!(slash_query("/review now", 4), Some("review".to_string()));
        assert_eq!(slash_query("/review now", 9), None);
        assert_eq!(slash_query("hi /model", 9), None);
    }

    #[test]
    fn quotes_paths_with_whitespace() {
        assert_eq!(quote_path("src/main.rs"), "src/main.rs");
        assert_eq!(quote_path("My Docs/a b.txt"), "\"My Docs/a b.txt\"");
        assert_eq!(quote_path("odd \"name\".txt"), "odd \"name\".txt");
    }

    #[test]
    fn completion_replaces_token_and_adds_one_space() {
        assert_eq!(
            insert_completion("see @ma", 4..7, "src/main.rs"),
            ("see src/main.rs ".to_string(), 16)
        );
        assert_eq!(
            insert_completion("see @ma now", 4..7, "src/main.rs"),
            ("see src/main.rs now".to_string(), 16)
        );
        assert_eq!(
            insert_completion("@ma\nnext", 0..3, "a.rs"),
            ("a.rs \nnext".to_string(), 5)
        );
    }

    #[test]
    fn removes_token_and_following_space() {
        assert_eq!(
            remove_range("look @a.png at this", 5..11),
            ("look at this".to_string(), 5)
        );
        assert_eq!(remove_range("@a.png", 0..6), (String::new(), 0));
    }

    #[test]
    fn inserts_at_cursor() {
        assert_eq!(insert_at("ab", 1, "X"), ("aXb".to_string(), 2));
        assert_eq!(insert_at("ab", 10, "X"), ("abX".to_string(), 3));
    }

    #[test]
    fn extracts_dollar_mentions() {
        assert_eq!(
            dollar_mentions("use $skill-creator and ($review:), $skill-creator again"),
            vec!["skill-creator".to_string(), "review".to_string()]
        );
        assert_eq!(dollar_mentions("cost $5 a$b $$x $"), vec!["5".to_string()]);
        assert_eq!(
            dollar_mentions("$plugin:skill."),
            vec!["plugin:skill".to_string()]
        );
    }

    #[test]
    fn fuzzy_scores_prefix_substring_and_subsequence() {
        assert_eq!(fuzzy_score("Model", "mo"), Some((0, vec![0, 1])));
        assert_eq!(
            fuzzy_score("lint-rust", "rust"),
            Some((1, vec![5, 6, 7, 8]))
        );
        assert_eq!(fuzzy_score("debug-config", "dbg"), Some((2, vec![0, 2, 4])));
        assert_eq!(fuzzy_score("diff", "xyz"), None);
        assert_eq!(fuzzy_score("any", ""), Some((0, Vec::new())));
    }

    #[test]
    fn markdown_escaping_round_trips_through_styled_text() {
        let path = "src/[a]_b*c<d>`e`.rs";
        let markdown = highlighted_markdown(path, &[0, 1, 4, 5], "#2f6fe0");
        assert_eq!(
            markdown,
            "<font color=\"#2f6fe0\">sr</font>c\\/<font color=\"#2f6fe0\">\\[a</font>\\]\\_b\\*c\\<d\\>\\`e\\`\\.rs"
        );
        assert!(slint::StyledText::from_markdown(&markdown).is_ok());
        assert!(slint::StyledText::from_markdown(&escape_markdown("# not a heading\n- x")).is_ok());
    }
}
