//! Codex App directives embedded in assistant markdown (port of the TUI's
//! `git_action_directives` / `inline_directives`).
//!
//! - `::code-comment{title=".." body=".." file=".." start=N end=M priority=P}`
//!   lines become a list item `- [Pp] title — file:start-end` plus the body.
//! - `::git-stage|git-commit|git-create-branch|git-push|git-create-pr{…}` and
//!   `::codex-inline-vis{…}` are removed (the GUI has no receipts for them).
//! - `:codex-file-citation{path="src/x.rs:12"}` becomes a link to the file.
//! - `:codex-followup[Label]{…}` shows only `Label`.
//!
//! Lines inside fenced code blocks and text inside code spans are untouched.

use std::path::Path;

use super::links::display_path;
use super::markdown::FenceTracker;
use super::markdown::code_span;
use super::markdown::link_destination;

/// Prefixes of the directives this module rewrites.
const DIRECTIVE_MARKERS: [&str; 4] = ["::git-", "::code-comment", "::codex-inline-vis", ":codex-"];

/// A parsed `::name[label]{key="value" …}` directive.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Directive<'a> {
    pub(crate) name: &'a str,
    pub(crate) label: Option<&'a str>,
    pub(crate) attributes: Vec<(&'a str, String)>,
    /// Byte length of the directive source.
    pub(crate) len: usize,
}

impl Directive<'_> {
    pub(crate) fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.as_str())
    }
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

/// Parses a directive at the start of `source` (1–3 leading colons).
pub(crate) fn parse_directive(source: &str) -> Option<Directive<'_>> {
    let rest = source.trim_start_matches(':');
    let colons = source.len() - rest.len();
    if !(1..=3).contains(&colons) {
        return None;
    }
    let name_len = rest.bytes().take_while(|byte| is_name_byte(*byte)).count();
    let (name, mut rest) = rest.split_at(name_len);
    if !name
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic())
    {
        return None;
    }
    let mut label = None;
    if let Some(after) = rest.strip_prefix('[') {
        let mut depth = 1;
        let mut end = None;
        let mut escaped = false;
        for (index, ch) in after.char_indices() {
            match ch {
                '\n' | '\r' => return None,
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end?;
        label = Some(&after[..end]);
        rest = &after[end + 1..];
    }
    let mut rest = rest.strip_prefix('{')?;
    let mut attributes = Vec::new();
    loop {
        rest = rest.trim_start_matches([' ', '\t']);
        if let Some(after) = rest.strip_prefix('}') {
            return Some(Directive {
                name,
                label,
                attributes,
                len: source.len() - after.len(),
            });
        }
        let key_len = rest.bytes().take_while(|byte| is_name_byte(*byte)).count();
        if key_len == 0 {
            return None;
        }
        let (key, after_key) = rest.split_at(key_len);
        let after_eq = after_key
            .trim_start_matches([' ', '\t'])
            .strip_prefix('=')?;
        let value_source = after_eq.trim_start_matches([' ', '\t']);
        let (value, remaining) = match value_source.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let mut value = String::new();
                let mut chars = value_source[1..].char_indices();
                let mut end = None;
                while let Some((index, ch)) = chars.next() {
                    match ch {
                        '\\' => match chars.next() {
                            Some((_, next)) if next == quote || next == '\\' => value.push(next),
                            Some((_, next)) => {
                                value.push('\\');
                                value.push(next);
                            }
                            None => return None,
                        },
                        '\n' => return None,
                        _ if ch == quote => {
                            end = Some(index);
                            break;
                        }
                        _ => value.push(ch),
                    }
                }
                let end = end?;
                (value, &value_source[1 + end + 1..])
            }
            _ => {
                let len = value_source
                    .find(|ch: char| ch.is_whitespace() || ch == '}')
                    .unwrap_or(value_source.len());
                if len == 0 {
                    return None;
                }
                (value_source[..len].to_string(), &value_source[len..])
            }
        };
        if attributes.iter().any(|(existing, _)| *existing == key) {
            return None;
        }
        attributes.push((key, value));
        rest = remaining;
    }
}

/// Rewrites assistant markdown for display, line by line.
pub(crate) fn visible_markdown(source: &str, cwd: &Path) -> String {
    let mut preprocessor = LinePreprocessor::default();
    let mut out = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        out.push_str(&preprocessor.line(line, cwd));
    }
    // Trailing blank lines carry no content.
    let trimmed = out.trim_end_matches(['\n', ' ', '\t', '\r']).len();
    out.truncate(trimmed);
    out
}

/// Stateful per-line preprocessing (tracks code fences across lines).
#[derive(Clone, Debug, Default)]
pub(crate) struct LinePreprocessor {
    fence: FenceTracker,
}

impl LinePreprocessor {
    /// Rewrites one line; `line` may include its trailing newline.
    pub(crate) fn line(&mut self, line: &str, cwd: &Path) -> String {
        let (content, newline) = match line.strip_suffix('\n') {
            Some(content) => (content.strip_suffix('\r').unwrap_or(content), "\n"),
            None => (line, ""),
        };
        let was_in_fence = self.fence.in_fence();
        self.fence.advance(content);
        if was_in_fence || self.fence.in_fence() {
            return format!("{content}{newline}");
        }
        let rewritten = rewrite_line(content, cwd);
        match rewritten {
            Some(rewritten) => format!("{}{newline}", rewritten.trim_end()),
            None => format!("{content}{newline}"),
        }
    }

    /// Previews an incomplete line without advancing the fence state.
    /// Returns `None` while a directive in it may still be incomplete.
    pub(crate) fn preview(&self, partial: &str, cwd: &Path) -> Option<String> {
        if self.fence.in_fence() {
            return Some(partial.to_string());
        }
        let may_hold_directive =
            |text: &str| DIRECTIVE_MARKERS.iter().any(|marker| text.contains(marker));
        if !may_hold_directive(partial) {
            return Some(partial.to_string());
        }
        let rewritten = rewrite_line(partial, cwd)?;
        (!may_hold_directive(&rewritten)).then_some(rewritten)
    }

    pub(crate) fn in_fence(&self) -> bool {
        self.fence.in_fence()
    }
}

/// Returns the rewritten line when it contained directives.
fn rewrite_line(line: &str, cwd: &Path) -> Option<String> {
    if !line.contains(':') {
        return None;
    }
    let content = line.trim_start_matches([' ', '\t']);
    let indent = &line[..line.len() - content.len()];
    if let Some(directive) = parse_directive(content)
        && directive.name == "code-comment"
        && let Some(rewritten) = code_comment(&directive, indent, cwd)
    {
        let suffix = strip_leaf_directives(&content[directive.len..]);
        return Some(format!("{rewritten}{suffix}"));
    }
    let stripped = strip_leaf_directives(line);
    let inline = rewrite_inline_directives(&stripped, cwd, /*labels_only*/ false);
    (inline != line).then_some(inline)
}

fn code_comment(directive: &Directive<'_>, indent: &str, cwd: &Path) -> Option<String> {
    let title = directive.attribute("title")?.trim();
    let body = directive.attribute("body")?.trim();
    let file = directive.attribute("file")?.trim();
    if title.is_empty() || body.is_empty() || file.is_empty() {
        return None;
    }
    let number = |key: &str| -> Option<i64> {
        directive
            .attribute(key)?
            .trim()
            .trim_start_matches(['P', 'p'])
            .parse()
            .ok()
    };
    let start = number("start").unwrap_or(1).max(1);
    let end = number("end").unwrap_or(start).max(start);
    let has_priority = {
        let bytes = title.as_bytes();
        bytes.len() >= 4
            && bytes[0] == b'['
            && matches!(bytes[1], b'P' | b'p')
            && bytes[2].is_ascii_digit()
            && bytes[3] == b']'
    };
    let title = match number("priority") {
        Some(priority @ 0..=3) if !has_priority => format!("[P{priority}] {title}"),
        _ => title.to_string(),
    };
    let file = display_path(file, cwd).replace('\\', "/");
    let location = if start == end {
        format!("{file}:{start}")
    } else {
        format!("{file}:{start}-{end}")
    };
    Some(format!("{indent}- {title} — {location}\n{indent}  {body}"))
}

/// Removes `::git-*{…}` and `::codex-inline-vis{…}` leaf directives.
fn strip_leaf_directives(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    loop {
        let next = ["::git-", "::codex-inline-vis"]
            .iter()
            .filter_map(|marker| rest.find(marker))
            .min();
        let Some(start) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..start]);
        let source = &rest[start..];
        if let Some(directive) = parse_directive(source) {
            rest = &source[directive.len..];
        } else if let Some((_, after)) = source.split_once('{')
            && let Some((_, suffix)) = after.split_once('}')
        {
            rest = suffix;
        } else {
            out.push_str(source);
            return out;
        }
    }
}

/// Rewrites inline `:codex-file-citation{…}` and `:codex-followup[…]{…}`
/// directives outside code spans. With `labels_only`, citations are kept.
fn rewrite_inline_directives(line: &str, cwd: &Path, labels_only: bool) -> String {
    if !line.contains(":codex-") {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    let mut index = 0;
    let bytes = line.as_bytes();
    while index < line.len() {
        let byte = bytes[index];
        if byte == b'`' {
            // Copy the whole code span verbatim.
            let run = line[index..].bytes().take_while(|b| *b == b'`').count();
            let fence = &line[index..index + run];
            match line[index + run..].find(fence) {
                Some(close) => {
                    let end = index + run + close + run;
                    out.push_str(&line[index..end]);
                    index = end;
                }
                None => {
                    out.push_str(&line[index..index + run]);
                    index += run;
                }
            }
            continue;
        }
        if byte == b':'
            && line[index..].starts_with(":codex-")
            && !line[..index].ends_with(':')
            && let Some(directive) = parse_directive(&line[index..])
        {
            match (directive.name, directive.label) {
                ("codex-followup", Some(label)) => {
                    out.push_str(label);
                    index += directive.len;
                    continue;
                }
                ("codex-file-citation", _) if !labels_only => {
                    if let Some(path) = directive.attribute("path") {
                        let shown = display_path(path, cwd);
                        out.push_str(&format!(
                            "[{}]({})",
                            code_span(&shown),
                            link_destination(path)
                        ));
                        index += directive.len;
                        continue;
                    }
                }
                _ => {}
            }
        }
        let ch_len = line[index..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&line[index..index + ch_len]);
        index += ch_len;
    }
    out
}

/// Markdown for copying: follow-up directives replaced by their labels,
/// everything else unchanged.
pub(crate) fn followup_labels(markdown: &str) -> String {
    if !markdown.contains(":codex-followup[") {
        return markdown.to_string();
    }
    markdown
        .split_inclusive('\n')
        .map(|line| rewrite_inline_directives(line, Path::new(""), /*labels_only*/ true))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_quoted_and_bare_attributes() {
        let directive = parse_directive(r#"::git-push{cwd="/re\"po" branch=main} tail"#);
        let directive = directive.as_ref();
        assert_eq!(directive.map(|d| d.name), Some("git-push"));
        assert_eq!(directive.and_then(|d| d.attribute("cwd")), Some("/re\"po"));
        assert_eq!(directive.and_then(|d| d.attribute("branch")), Some("main"));
        assert_eq!(
            directive.map(|d| d.len),
            Some(r#"::git-push{cwd="/re\"po" branch=main}"#.len())
        );
        assert_eq!(parse_directive("::{x=1}"), None);
        assert_eq!(parse_directive("::::git{}"), None);
    }

    #[test]
    fn strips_git_directives_and_rewrites_code_comments() {
        let cwd = Path::new("/repo");
        let source = "Done.\n::git-stage{cwd=\"/repo\"} ::git-commit{cwd=\"/repo\"}\n::code-comment{title=\"Bug\" body=\"Fix it\" file=\"/repo/src/a.rs\" start=3 end=5 priority=1}\n";
        assert_eq!(
            visible_markdown(source, cwd),
            "Done.\n\n- [P1] Bug — src/a.rs:3-5\n  Fix it"
        );
    }

    #[test]
    fn inline_directives_become_links_and_labels() {
        let cwd = Path::new("/repo");
        let source = "See :codex-file-citation{path=\"src/x.rs:12\"} and :codex-followup[Run tests]{id=\"a\"} but not `:codex-followup[x]{}`";
        assert_eq!(
            visible_markdown(source, cwd),
            "See [`src/x.rs:12`](<src/x.rs:12>) and Run tests but not `:codex-followup[x]{}`"
        );
        assert_eq!(
            followup_labels("Try :codex-followup[this]{a=1}."),
            "Try this."
        );
    }

    #[test]
    fn windows_citations_keep_their_backslashes() {
        // `\.` and `\_` are CommonMark escapes; the path must survive the
        // markdown round trip unchanged.
        let source = r#"See :codex-file-citation{path="C:\\repo\\.github\\ci.yml:3"}."#;
        let markdown = visible_markdown(source, Path::new("/repo"));
        let dests: Vec<String> = pulldown_cmark::Parser::new(&markdown)
            .filter_map(|event| match event {
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::Link { dest_url, .. }) => {
                    Some(dest_url.into_string())
                }
                _ => None,
            })
            .collect();
        assert_eq!(dests, vec![r"C:\repo\.github\ci.yml:3".to_string()]);
    }

    #[test]
    fn code_fences_are_left_alone() {
        let source = "```\n::git-push{cwd=\"/x\" branch=b}\n```\n";
        assert_eq!(
            visible_markdown(source, Path::new("/")),
            "```\n::git-push{cwd=\"/x\" branch=b}\n```"
        );
    }

    #[test]
    fn preview_withholds_incomplete_directives() {
        let preprocessor = LinePreprocessor::default();
        assert_eq!(
            preprocessor.preview("text ::git-push{cwd=\"/x", Path::new("/")),
            None
        );
        assert_eq!(
            preprocessor.preview("plain text", Path::new("/")),
            Some("plain text".to_string())
        );
        // Rust paths are not directives.
        assert_eq!(
            preprocessor.preview("use std::fs::read", Path::new("/")),
            Some("use std::fs::read".to_string())
        );
    }
}
