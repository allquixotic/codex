//! Incremental markdown rendering for streamed agent messages and plans
//! (port of the TUI's stable-region / mutable-tail controller).
//!
//! Deltas accumulate in `raw`. Only complete lines are committed; committed
//! lines are preprocessed (directives) into `visible`. Top-level blocks that
//! are followed by another top-level block can no longer change, so they are
//! rendered once and kept in `stable`. Each update re-parses only the source
//! after the stable boundary (the last block plus the incomplete line).
//!
//! The last block can grow without bound when it is a code fence (a model
//! writing a whole file). While a top-level fence is open, its content is
//! not parsed at all: rows of [`CODE_CHUNK_LINES`] complete lines become
//! stable as soon as more code follows them, and only the last row is
//! rebuilt, so neither the Rust side nor the text layout redoes the whole
//! block on every delta. (A long single paragraph or list is still
//! re-parsed whole, but only rows that changed reach the view.)
//!
//! The incomplete last line is shown as a preview only when it cannot turn
//! into block structure (tables, fences, quotes, indented code), mirroring
//! the TUI's prose preview rules. `item/completed` text is authoritative and
//! replaces everything streamed (see `render.rs`).

use std::path::Path;
use std::path::PathBuf;

use super::blocks::Block;
use super::blocks::Part;
use super::directives::LinePreprocessor;
use super::markdown::BlockStyle;
use super::markdown::CODE_CHUNK_LINES;
use super::markdown::MAX_CODE_LINES;
use super::markdown::code_blocks_after;
use super::markdown::code_language;
use super::markdown::code_row;
use super::markdown::fence_marker;
use super::markdown::is_table_header_line;
use super::markdown::node_blocks;
use super::markdown::parse;

/// Largest incomplete line rendered as a preview (the tail of it is shown).
const MAX_PREVIEW_BYTES: usize = 8192;

/// A top-level code fence that is still open.
#[derive(Clone, Debug)]
struct OpenFence {
    lang: String,
    /// Spaces before the opening fence, removed from every content line.
    indent: usize,
    /// Offset in `visible` of the first content line not in a stable row.
    rest: usize,
    /// Rows of the fence at the end of `stable`.
    rows: usize,
    /// Lines in those rows.
    lines: usize,
}

/// Streaming state of one markdown item.
#[derive(Clone, Debug)]
pub(crate) struct MarkdownStream {
    raw: String,
    committed_raw: usize,
    visible: String,
    preprocessor: LinePreprocessor,
    /// `visible[..stable_src]` holds only completed top-level blocks.
    stable_src: usize,
    stable: Vec<Block>,
    tail: Vec<Block>,
    /// A reference definition was seen: later text can change earlier links,
    /// so everything is re-rendered on each update.
    full_reparse: bool,
    /// The pending source is an open top-level code fence.
    fence: Option<OpenFence>,
    style: BlockStyle,
    cwd: PathBuf,
}

impl MarkdownStream {
    pub(crate) fn new(style: BlockStyle, cwd: &Path) -> Self {
        Self {
            raw: String::new(),
            committed_raw: 0,
            visible: String::new(),
            preprocessor: LinePreprocessor::default(),
            stable_src: 0,
            stable: Vec::new(),
            tail: Vec::new(),
            full_reparse: false,
            fence: None,
            style,
            cwd: cwd.to_path_buf(),
        }
    }

    /// Everything received so far.
    pub(crate) fn raw(&self) -> &str {
        &self.raw
    }

    /// Number of leading blocks that will never change again.
    #[cfg(test)]
    pub(crate) fn stable_len(&self) -> usize {
        self.stable.len()
    }

    /// Current blocks: the stable prefix followed by the re-rendered tail.
    pub(crate) fn blocks(&self) -> Vec<Block> {
        let mut blocks = Vec::with_capacity(self.stable.len() + self.tail.len());
        blocks.extend(self.stable.iter().cloned());
        blocks.extend(self.tail.iter().cloned());
        blocks
    }

    /// Appends a delta; returns true when the visible blocks changed.
    pub(crate) fn push(&mut self, delta: &str) -> bool {
        if delta.is_empty() {
            return false;
        }
        self.raw.push_str(delta);
        let stable_before = self.stable.len();
        if let Some(newline) = self.raw[self.committed_raw..].rfind('\n') {
            let end = self.committed_raw + newline + 1;
            let mut fence_closed = false;
            for line in self.raw[self.committed_raw..end].split_inclusive('\n') {
                let visible = self.preprocessor.line(line, &self.cwd);
                self.visible.push_str(&visible);
                fence_closed |= self.fence.is_some() && !self.preprocessor.in_fence();
            }
            self.committed_raw = end;
            if fence_closed {
                // The closed block is parsed like any other from here on;
                // its rows render the same as the stable ones dropped here.
                if let Some(fence) = self.fence.take() {
                    self.stable.truncate(self.stable.len() - fence.rows);
                }
            }
            if self.fence.is_none() {
                self.advance_stable();
            }
            self.advance_fence();
        }
        let tail = self.render_tail();
        let changed = tail != self.tail || self.stable.len() != stable_before;
        self.tail = tail;
        changed
    }

    /// Notices a top-level code fence opening at the stable boundary, and
    /// makes its rows stable once more code follows them.
    fn advance_fence(&mut self) {
        if self.full_reparse || !self.preprocessor.in_fence() {
            return;
        }
        if self.fence.is_none() {
            // The parser reports an indented fence as starting at its
            // backticks: measure the indentation from the line start.
            let line_start = self.visible[..self.stable_src]
                .rfind('\n')
                .map_or(0, |newline| newline + 1);
            let pending = &self.visible[line_start..];
            let Some(opener) = pending.split_inclusive('\n').next() else {
                return;
            };
            let line = opener.trim_end_matches(['\n', '\r']);
            let indent = line.len() - line.trim_start_matches(' ').len();
            // Fences in quotes and list items are part of a larger block.
            if indent > 3 || !line[indent..].starts_with(['`', '~']) {
                return;
            }
            let Some((_, _, info)) = fence_marker(line) else {
                return;
            };
            self.fence = Some(OpenFence {
                lang: code_language(info),
                indent,
                rest: line_start + opener.len(),
                rows: 0,
                lines: 0,
            });
        }
        let Some(fence) = self.fence.as_mut() else {
            return;
        };
        while fence.lines + CODE_CHUNK_LINES <= MAX_CODE_LINES {
            let content = &self.visible[fence.rest..];
            let Some(end) = nth_line_end(content, CODE_CHUNK_LINES) else {
                break;
            };
            // Trailing blank lines are not part of a code block: wait until
            // code follows the row.
            if content[end..].trim_end_matches(['\n', '\r']).is_empty() {
                break;
            }
            let text = content[..end]
                .lines()
                .map(|line| strip_indent(line, fence.indent))
                .collect::<Vec<_>>()
                .join("\n");
            let part = if fence.rows == 0 {
                Part::First
            } else {
                Part::Middle
            };
            self.stable.push(code_row(
                &fence.lang,
                text,
                part,
                self.style,
                /*quote*/ 0,
                /*level*/ 0,
            ));
            fence.rows += 1;
            fence.lines += CODE_CHUNK_LINES;
            fence.rest += end;
        }
    }

    fn advance_stable(&mut self) {
        if self.full_reparse {
            return;
        }
        let pending = &self.visible[self.stable_src..];
        let parsed = parse(pending);
        if parsed.has_reference_definitions {
            self.full_reparse = true;
            self.stable.clear();
            self.stable_src = 0;
            return;
        }
        let hold = bare_list_marker_start(pending);
        let mut boundary_index = parsed.nodes.len().saturating_sub(1);
        while boundary_index > 0 {
            match parsed.nodes[boundary_index].0 {
                Some(start) if hold.is_none_or(|hold| start <= hold) => break,
                _ => boundary_index -= 1,
            }
        }
        if boundary_index == 0 {
            return;
        }
        let Some(boundary) = parsed.nodes[boundary_index].0 else {
            return;
        };
        // The parser reports an indented block at its first non-space
        // character; keep the indentation with the block, or a re-parse
        // from the boundary would read it differently.
        let line_start = pending[..boundary]
            .rfind('\n')
            .map_or(0, |newline| newline + 1);
        let boundary = if pending[line_start..boundary]
            .bytes()
            .all(|byte| byte == b' ')
        {
            line_start
        } else {
            boundary
        };
        for (_, node) in &parsed.nodes[..boundary_index] {
            node_blocks(
                node,
                self.style,
                /*quote*/ 0,
                /*level*/ 0,
                &mut self.stable,
            );
        }
        self.stable_src += boundary;
    }

    fn render_tail(&self) -> Vec<Block> {
        let partial = &self.raw[self.committed_raw..];
        if let Some(fence) = &self.fence {
            let mut text = String::new();
            for line in self.visible[fence.rest..].split_inclusive('\n') {
                text.push_str(strip_indent(line, fence.indent));
            }
            if !partial.is_empty()
                && let Some(preview) = self.preview(partial, "")
            {
                text.push_str(strip_indent(&preview, fence.indent));
            }
            let mut blocks = Vec::new();
            code_blocks_after(
                &fence.lang,
                &text,
                fence.rows,
                fence.lines,
                self.style,
                &mut blocks,
            );
            return blocks;
        }
        let mut source = self.visible[self.stable_src..].to_string();
        if !partial.is_empty()
            && let Some(preview) = self.preview(partial, &source)
        {
            source.push_str(&preview);
        }
        let mut blocks = Vec::new();
        for (_, node) in parse(&source).nodes {
            node_blocks(
                &node,
                self.style,
                /*quote*/ 0,
                /*level*/ 0,
                &mut blocks,
            );
        }
        blocks
    }

    /// The incomplete last line as it may be shown now, if at all.
    fn preview(&self, partial: &str, pending: &str) -> Option<String> {
        let partial = partial.strip_suffix('\r').unwrap_or(partial);
        if self.preprocessor.in_fence() {
            let trimmed = partial.trim_start();
            if trimmed.starts_with('`') || trimmed.starts_with('~') {
                return None;
            }
            return Some(crop_preview(partial).to_string());
        }
        if !prose_preview_allowed(partial) {
            return None;
        }
        let last_line = pending
            .trim_end_matches('\n')
            .rsplit('\n')
            .next()
            .unwrap_or("");
        if last_line.contains('|') && is_table_header_line(last_line) {
            return None;
        }
        let preview = self.preprocessor.preview(partial, &self.cwd)?;
        Some(trim_unfinished_inline(crop_preview(&preview)).to_string())
    }
}

/// Whether an incomplete line is plain prose that cannot become block
/// structure once more text arrives.
pub(crate) fn prose_preview_allowed(partial: &str) -> bool {
    if partial.starts_with([' ', '\t', '>', '|']) || partial.contains('|') {
        return false;
    }
    if matches!(partial, "`" | "``" | "~" | "~~") || fence_marker(partial).is_some() {
        return false;
    }
    true
}

/// Withholds an unfinished link destination or code span from a preview.
fn trim_unfinished_inline(preview: &str) -> &str {
    let mut end = preview.len();
    if let Some(open) = preview.rfind("](")
        && !preview[open..].contains(')')
    {
        end = preview[..open].rfind('[').unwrap_or(open);
    }
    let shown = &preview[..end];
    if shown.matches('`').count() % 2 == 1
        && let Some(last) = shown.rfind('`')
    {
        return &shown[..last];
    }
    shown
}

/// Byte offset just past the `n`-th newline of `text`.
fn nth_line_end(text: &str, n: usize) -> Option<usize> {
    text.match_indices('\n')
        .nth(n.checked_sub(1)?)
        .map(|(index, _)| index + 1)
}

/// `line` without up to `indent` leading spaces (the indentation of the
/// fence it is in).
fn strip_indent(line: &str, indent: usize) -> &str {
    let spaces = line.len() - line.trim_start_matches(' ').len();
    &line[spaces.min(indent)..]
}

fn crop_preview(preview: &str) -> &str {
    if preview.len() <= MAX_PREVIEW_BYTES {
        return preview;
    }
    let mut start = preview.len() - MAX_PREVIEW_BYTES;
    while !preview.is_char_boundary(start) {
        start += 1;
    }
    &preview[start..]
}

/// Offset of the last committed line when it is a bare list marker (`-`,
/// `3.`), which the next line can still turn into a list item.
fn bare_list_marker_start(pending: &str) -> Option<usize> {
    let body = pending.strip_suffix('\n')?;
    let start = body.rfind('\n').map_or(0, |index| index + 1);
    let line = body[start..].trim().trim_start_matches(['>', ' ', '\t']);
    let bare = matches!(line, "-" | "+" | "*")
        || line
            .strip_suffix(['.', ')'])
            .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()));
    bare.then_some(start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::blocks::BlockKind;
    use crate::transcript::markdown::render;
    use pretty_assertions::assert_eq;

    fn stream() -> MarkdownStream {
        MarkdownStream::new(BlockStyle::default(), Path::new("/repo"))
    }

    fn kinds(blocks: &[Block]) -> Vec<BlockKind> {
        blocks.iter().map(|block| block.kind).collect()
    }

    #[test]
    fn completed_blocks_become_stable() {
        let mut stream = stream();
        stream.push("# Title\n\nFirst para");
        // Only one complete block so far: nothing is final yet.
        assert_eq!(stream.stable_len(), 0);
        assert_eq!(
            kinds(&stream.blocks()),
            vec![BlockKind::Heading, BlockKind::Paragraph]
        );
        stream.push("graph.\n\nSecond");
        // The heading is followed by another block now.
        assert_eq!(stream.stable_len(), 1);
        stream.push(" paragraph.\n\nThird");
        assert_eq!(stream.stable_len(), 2);
        assert_eq!(stream.blocks().len(), 4);
    }

    #[test]
    fn streamed_result_matches_full_render() {
        let source = "# Summary\n\nSome **bold** text.\n\n- a\n- b\n\n```rust\nfn x() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nDone.\n";
        let mut stream = stream();
        for chunk in source.as_bytes().chunks(5) {
            stream.push(std::str::from_utf8(chunk).unwrap_or_default());
        }
        assert_eq!(stream.blocks(), render(source, BlockStyle::default()));
    }

    #[test]
    fn partial_prose_is_previewed() {
        let mut stream = stream();
        stream.push("Hello wor");
        assert_eq!(
            stream.blocks().first().and_then(|block| block.rich.clone()),
            Some("Hello wor".to_string())
        );
    }

    #[test]
    fn partial_table_rows_are_withheld() {
        let mut stream = stream();
        stream.push("| a | b |\n|---|");
        let blocks = stream.blocks();
        assert_eq!(kinds(&blocks), vec![BlockKind::Paragraph]);
        assert_eq!(blocks[0].rich.as_deref(), Some("\\| a \\| b \\|"));
    }

    #[test]
    fn code_inside_open_fence_is_previewed() {
        let mut stream = stream();
        stream.push("```sh\nls -l");
        let blocks = stream.blocks();
        assert_eq!(kinds(&blocks), vec![BlockKind::Code]);
        assert_eq!(blocks[0].text, "ls -l");
    }

    #[test]
    fn long_open_fences_rebuild_only_their_last_row() {
        let code: String = (0..200).map(|index| format!("  line {index}\n")).collect();
        let source = format!("Intro.\n\n  ```rust\n{code}");
        let mut stream = stream();
        for chunk in source.as_bytes().chunks(7) {
            stream.push(std::str::from_utf8(chunk).unwrap_or_default());
            // Once the fence is open, only its last row is re-rendered (two
            // while the line after a full row is still incomplete).
            if stream.fence.is_some() {
                let lines: usize = stream.tail.iter().map(|row| row.text.lines().count()).sum();
                assert!(lines <= CODE_CHUNK_LINES + 1, "{lines}");
            }
        }
        // The paragraph and three full rows of code are final.
        assert_eq!(stream.stable_len(), 4);
        assert_eq!(stream.blocks(), render(&source, BlockStyle::default()));
        // A partial line shows in the last row.
        stream.push("  let x");
        let last = stream.blocks().last().map(|block| block.text.clone());
        assert!(last.is_some_and(|text| text.ends_with("line 199\nlet x")));
        // Closing the fence and going on renders like the whole message.
        stream.push(" = 1;\n  ```\n\nDone.\n");
        let whole = format!("{source}  let x = 1;\n  ```\n\nDone.\n");
        assert_eq!(stream.blocks(), render(&whole, BlockStyle::default()));
    }

    #[test]
    fn streamed_fences_past_the_display_limit_match_the_full_render() {
        let code: String = (0..MAX_CODE_LINES + 70)
            .map(|index| format!("{index}\n"))
            .collect();
        let source = format!("~~~\n{code}");
        let mut stream = stream();
        for chunk in source.as_bytes().chunks(61) {
            stream.push(std::str::from_utf8(chunk).unwrap_or_default());
        }
        assert_eq!(stream.blocks(), render(&source, BlockStyle::default()));
        stream.push("~~~\nend\n");
        let whole = format!("{source}~~~\nend\n");
        assert_eq!(stream.blocks(), render(&whole, BlockStyle::default()));
    }

    #[test]
    fn fences_in_quotes_and_lists_are_parsed_normally() {
        let source = "> ```\n> a\n> b\n";
        let mut stream = stream();
        stream.push(source);
        assert!(stream.fence.is_none());
        assert_eq!(stream.blocks(), render(source, BlockStyle::default()));
    }

    #[test]
    fn unfinished_links_and_code_are_trimmed() {
        assert_eq!(trim_unfinished_inline("see [docs](http"), "see ");
        assert_eq!(trim_unfinished_inline("run `cargo"), "run ");
        assert_eq!(trim_unfinished_inline("ok [a](b) `c`"), "ok [a](b) `c`");
    }

    #[test]
    fn bare_list_markers_hold_the_boundary() {
        assert_eq!(bare_list_marker_start("text\n\n-\n"), Some(6));
        assert_eq!(bare_list_marker_start("text\n12.\n"), Some(5));
        assert_eq!(bare_list_marker_start("text\n- item\n"), None);
    }

    #[test]
    fn reference_definitions_force_full_rendering() {
        let mut stream = stream();
        stream.push("First [link][x].\n\nSecond.\n\n[x]: https://a.b\n");
        assert_eq!(stream.stable_len(), 0);
        let blocks = stream.blocks();
        assert_eq!(
            blocks[0].rich.as_deref(),
            Some("First [link](<https://a.b>)\\.")
        );
    }

    #[test]
    fn prose_preview_rules() {
        assert!(prose_preview_allowed("Some text"));
        assert!(!prose_preview_allowed("| a"));
        assert!(!prose_preview_allowed("    code"));
        assert!(!prose_preview_allowed("```"));
        assert!(!prose_preview_allowed("> quote"));
    }
}
