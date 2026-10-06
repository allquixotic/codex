//! Markdown → transcript blocks.
//!
//! Agent text is parsed with pulldown-cmark into a small block tree. Block
//! structure that Slint's `StyledText` cannot show (headings, code, tables,
//! quotes, rules) becomes separate rows; inline content is re-serialized into
//! a *sanitized* markdown string that only uses the constructs
//! `StyledText::from_markdown` accepts (emphasis, strong, strikethrough,
//! inline code, links, lists) with every other punctuation character
//! backslash-escaped. That makes `Vec<String>`, stray HTML, images, or task
//! markers render as text instead of failing the whole paragraph.
//!
//! Soft breaks stay line breaks (LLM output relies on them), matching the TUI.

use std::borrow::Cow;

use pulldown_cmark::Alignment;
use pulldown_cmark::CodeBlockKind;
use pulldown_cmark::Event;
use pulldown_cmark::HeadingLevel;
use pulldown_cmark::Options;
use pulldown_cmark::Parser;
use pulldown_cmark::Tag;

use super::blocks::Block;
use super::blocks::BlockKind;
use super::blocks::Gap;
use super::blocks::Part;
use super::blocks::Table;
use super::blocks::Tone;
use super::links::autolink_ranges;
use super::links::citation_destination;

/// Code blocks longer than this are cut for display (copy keeps everything).
pub(crate) const MAX_CODE_LINES: usize = 2000;
/// Lines per row of a long code block. A streamed block re-renders only its
/// last row, and the list lays out only the rows on screen.
pub(crate) const CODE_CHUNK_LINES: usize = 64;
/// Characters per column counted when estimating table column widths.
const MAX_COLUMN_WEIGHT: usize = 48;
const MIN_COLUMN_WEIGHT: usize = 3;

/// Styling applied to every block produced from one markdown source.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct BlockStyle {
    pub(crate) tone: Tone,
    /// Rows offer message-level actions (agent replies).
    pub(crate) message: bool,
}

/// Inline content of a paragraph, heading, or table cell.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Inline {
    Text(String),
    Code(String),
    Emphasis(Vec<Inline>),
    Strong(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    Link { dest: String, children: Vec<Inline> },
    Break,
    Html(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ListItem {
    pub(crate) task: Option<bool>,
    pub(crate) children: Vec<Node>,
}

/// Block-level markdown element.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Node {
    Paragraph(Vec<Inline>),
    Heading(u8, Vec<Inline>),
    Code {
        lang: String,
        text: String,
    },
    Quote(Vec<Node>),
    List {
        start: Option<u64>,
        items: Vec<ListItem>,
    },
    Table {
        aligns: Vec<Alignment>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    Rule,
    Html(String),
}

/// Result of parsing a markdown source.
#[derive(Debug, Default)]
pub(crate) struct ParsedMarkdown {
    /// Top-level nodes with their start offset in the *input* source, when
    /// the offset maps back unambiguously (see [`unwrap_markdown_fences`]).
    pub(crate) nodes: Vec<(Option<usize>, Node)>,
    /// `[label]: url` definitions exist; later text can change earlier links.
    pub(crate) has_reference_definitions: bool,
}

pub(crate) fn parser_options() -> Options {
    Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS
}

/// Parses `source` into top-level nodes.
pub(crate) fn parse(source: &str) -> ParsedMarkdown {
    let normalized = unwrap_markdown_fences(source);
    let parser = Parser::new_ext(&normalized, parser_options());
    let has_reference_definitions = parser.reference_definitions().iter().next().is_some();
    let mut builder = TreeBuilder::default();
    for (event, range) in parser.into_offset_iter() {
        builder.event(event, range.start);
    }
    let nodes = builder
        .finish()
        .into_iter()
        .map(|(start, node)| (map_offset(source, &normalized, start), node))
        .collect();
    ParsedMarkdown {
        nodes,
        has_reference_definitions,
    }
}

/// Renders a complete markdown source into blocks.
pub(crate) fn render(source: &str, style: BlockStyle) -> Vec<Block> {
    let mut blocks = Vec::new();
    for (_, node) in parse(source).nodes {
        node_blocks(
            &node,
            style,
            /*quote*/ 0,
            /*level*/ 0,
            &mut blocks,
        );
    }
    blocks
}

/// Maps a byte offset in the normalized source back to the original one.
fn map_offset(source: &str, normalized: &str, offset: usize) -> Option<usize> {
    if std::ptr::eq(source, normalized) {
        return Some(offset);
    }
    let suffix = normalized.get(offset..)?;
    source
        .strip_suffix(suffix)
        .map(str::len)
        .filter(|_| offset < normalized.len())
}

// ----- tree building ------------------------------------------------------

enum Frame {
    Paragraph(Vec<Inline>),
    Heading(u8, Vec<Inline>),
    Quote(Vec<Node>),
    Code(String, String),
    Html(String),
    List(Option<u64>, Vec<ListItem>),
    Item {
        task: Option<bool>,
        children: Vec<Node>,
        inlines: Vec<Inline>,
    },
    Table {
        aligns: Vec<Alignment>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    Row(Vec<Vec<Inline>>),
    Cell(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Strong(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    Link(String, Vec<Inline>),
    Image(Vec<Inline>),
    Ignore,
}

#[derive(Default)]
struct TreeBuilder {
    stack: Vec<Frame>,
    top: Vec<(usize, Node)>,
    top_start: usize,
}

impl TreeBuilder {
    fn event(&mut self, event: Event<'_>, start: usize) {
        if self.stack.is_empty() && matches!(event, Event::Start(_) | Event::Rule | Event::Html(_))
        {
            self.top_start = start;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(_) => self.end(),
            Event::Text(text) => match self.stack.last_mut() {
                Some(Frame::Code(_, body) | Frame::Html(body)) => body.push_str(&text),
                _ => self.inline(Inline::Text(text.into_string())),
            },
            Event::Code(code) => self.inline(Inline::Code(code.into_string())),
            Event::Html(html) => match self.stack.last_mut() {
                Some(Frame::Html(body)) => body.push_str(&html),
                None => self.node(Node::Html(html.into_string())),
                _ => self.inline(Inline::Html(html.into_string())),
            },
            Event::InlineHtml(html) => self.inline(Inline::Html(html.into_string())),
            Event::FootnoteReference(label) => self.inline(Inline::Text(format!("[^{label}]"))),
            Event::SoftBreak | Event::HardBreak => self.inline(Inline::Break),
            Event::Rule => self.node(Node::Rule),
            Event::TaskListMarker(checked) => {
                if let Some(Frame::Item { task, .. }) = self.stack.last_mut() {
                    *task = Some(checked);
                }
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        let frame = match tag {
            Tag::Paragraph => Frame::Paragraph(Vec::new()),
            Tag::Heading { level, .. } => Frame::Heading(heading_level(level), Vec::new()),
            Tag::BlockQuote => Frame::Quote(Vec::new()),
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => code_language(&info),
                    CodeBlockKind::Indented => String::new(),
                };
                Frame::Code(lang, String::new())
            }
            Tag::HtmlBlock => Frame::Html(String::new()),
            Tag::List(start) => Frame::List(start, Vec::new()),
            Tag::Item => Frame::Item {
                task: None,
                children: Vec::new(),
                inlines: Vec::new(),
            },
            Tag::Table(aligns) => Frame::Table {
                aligns,
                rows: Vec::new(),
            },
            Tag::TableHead | Tag::TableRow => Frame::Row(Vec::new()),
            Tag::TableCell => Frame::Cell(Vec::new()),
            Tag::Emphasis => Frame::Emphasis(Vec::new()),
            Tag::Strong => Frame::Strong(Vec::new()),
            Tag::Strikethrough => Frame::Strikethrough(Vec::new()),
            Tag::Link { dest_url, .. } => Frame::Link(dest_url.into_string(), Vec::new()),
            Tag::Image { .. } => Frame::Image(Vec::new()),
            Tag::FootnoteDefinition(_) | Tag::MetadataBlock(_) => Frame::Ignore,
        };
        self.stack.push(frame);
    }

    fn end(&mut self) {
        let Some(frame) = self.stack.pop() else {
            return;
        };
        match frame {
            Frame::Paragraph(inlines) => self.node(Node::Paragraph(inlines)),
            Frame::Heading(level, inlines) => self.node(Node::Heading(level, inlines)),
            Frame::Quote(children) => self.node(Node::Quote(children)),
            Frame::Code(lang, text) => self.node(Node::Code { lang, text }),
            Frame::Html(text) => self.node(Node::Html(text)),
            Frame::List(start, items) => self.node(Node::List { start, items }),
            Frame::Item {
                task,
                mut children,
                inlines,
            } => {
                if !inlines.is_empty() {
                    children.push(Node::Paragraph(inlines));
                }
                if let Some(Frame::List(_, items)) = self.stack.last_mut() {
                    items.push(ListItem { task, children });
                }
            }
            Frame::Table { aligns, rows } => self.node(Node::Table { aligns, rows }),
            Frame::Row(cells) => {
                if let Some(Frame::Table { rows, .. }) = self.stack.last_mut() {
                    rows.push(cells);
                }
            }
            Frame::Cell(inlines) => {
                if let Some(Frame::Row(cells)) = self.stack.last_mut() {
                    cells.push(inlines);
                }
            }
            Frame::Emphasis(children) => self.inline(Inline::Emphasis(children)),
            Frame::Strong(children) => self.inline(Inline::Strong(children)),
            Frame::Strikethrough(children) => self.inline(Inline::Strikethrough(children)),
            Frame::Link(dest, children) => self.inline(Inline::Link { dest, children }),
            Frame::Image(alt) => {
                for inline in alt {
                    self.inline(inline);
                }
            }
            Frame::Ignore => {}
        }
    }

    fn node(&mut self, node: Node) {
        match self.stack.last_mut() {
            None => self.top.push((self.top_start, node)),
            Some(Frame::Quote(children)) => children.push(node),
            Some(Frame::Item {
                children, inlines, ..
            }) => {
                if !inlines.is_empty() {
                    children.push(Node::Paragraph(std::mem::take(inlines)));
                }
                children.push(node);
            }
            Some(Frame::Ignore) => {}
            Some(_) => {
                // Block inside an inline container cannot happen in valid
                // event streams; keep the text rather than dropping it.
                if let Node::Html(text) = node {
                    self.inline(Inline::Html(text));
                }
            }
        }
    }

    fn inline(&mut self, inline: Inline) {
        match self.stack.last_mut() {
            Some(
                Frame::Paragraph(inlines)
                | Frame::Heading(_, inlines)
                | Frame::Cell(inlines)
                | Frame::Emphasis(inlines)
                | Frame::Strong(inlines)
                | Frame::Strikethrough(inlines)
                | Frame::Link(_, inlines)
                | Frame::Image(inlines)
                | Frame::Item { inlines, .. },
            ) => inlines.push(inline),
            Some(Frame::Code(_, body) | Frame::Html(body)) => {
                body.push_str(&inline_plain(&[inline]))
            }
            Some(Frame::Quote(_)) | None => self.node(Node::Paragraph(vec![inline])),
            Some(_) => {}
        }
    }

    fn finish(mut self) -> Vec<(usize, Node)> {
        while !self.stack.is_empty() {
            self.end();
        }
        self.top
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// First token of a fence info string (` ```rust,ignore ` → `rust`).
pub(crate) fn code_language(info: &str) -> String {
    info.split([',', ' ', '\t'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

// ----- nodes → blocks -----------------------------------------------------

fn base_block(kind: BlockKind, style: BlockStyle, quote: i32, level: i32) -> Block {
    Block {
        kind,
        tone: style.tone,
        quote,
        level,
        message: style.message,
        gap: Gap::Block,
        ..Block::default()
    }
}

/// Appends the blocks for `node` to `out`.
pub(crate) fn node_blocks(
    node: &Node,
    style: BlockStyle,
    quote: i32,
    level: i32,
    out: &mut Vec<Block>,
) {
    let paragraph_kind = if quote > 0 {
        BlockKind::Quote
    } else {
        BlockKind::Paragraph
    };
    match node {
        Node::Paragraph(inlines) => {
            let markdown = emit_inlines(inlines, "");
            if markdown.trim().is_empty() {
                return;
            }
            let mut block = base_block(paragraph_kind, style, quote, level);
            block.rich = Some(markdown);
            out.push(block);
        }
        Node::Heading(heading, inlines) => {
            let mut block = base_block(BlockKind::Heading, style, quote, i32::from(*heading));
            let inner = emit_inlines_with(inlines, "", /*in_strong*/ true);
            block.rich = Some(if inner.trim().is_empty() {
                String::new()
            } else {
                format!("**{inner}**")
            });
            out.push(block);
        }
        Node::Code { lang, text } => code_blocks(lang, text, style, quote, level, out),
        Node::Quote(children) => {
            if children.iter().all(is_simple_quote_child) {
                let markdown = children
                    .iter()
                    .filter_map(|child| match child {
                        Node::Paragraph(inlines) => Some(emit_inlines(inlines, "")),
                        Node::List { start, items } => Some(list_markdown(*start, items, "")),
                        _ => None,
                    })
                    .filter(|markdown| !markdown.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                if markdown.is_empty() {
                    return;
                }
                let mut block = base_block(BlockKind::Quote, style, quote + 1, level);
                block.rich = Some(markdown);
                out.push(block);
            } else {
                for child in children {
                    node_blocks(child, style, quote + 1, level, out);
                }
            }
        }
        Node::List { start, items } => {
            if items.iter().all(is_simple_item) {
                let mut rows = Vec::new();
                list_rows(*start, items, /*depth*/ 0, &mut rows);
                for (index, (depth, markdown)) in rows.into_iter().enumerate() {
                    let mut block = base_block(
                        paragraph_kind,
                        style,
                        quote,
                        level + i32::try_from(depth).unwrap_or(0),
                    );
                    if index > 0 {
                        block.gap = Gap::Item;
                    }
                    block.rich = Some(markdown);
                    out.push(block);
                }
            } else {
                complex_list_blocks(*start, items, style, quote, level, out);
            }
        }
        Node::Table { aligns, rows } => {
            let mut block = base_block(BlockKind::Table, style, quote, level);
            block.table = Some(table(aligns, rows));
            block.copyable = true;
            out.push(block);
        }
        Node::Rule => out.push(base_block(BlockKind::Rule, style, quote, level)),
        Node::Html(text) => {
            let text = text.trim_end();
            if text.is_empty() {
                return;
            }
            let mut block = base_block(paragraph_kind, style, quote, level);
            block.rich = Some(text.lines().map(escape_text).collect::<Vec<_>>().join("\n"));
            out.push(block);
        }
    }
}

/// Appends the rows of a code block: [`CODE_CHUNK_LINES`] lines per row
/// (see [`Part`]), at most [`MAX_CODE_LINES`] lines, then a note row with
/// the number of lines left out, whose Copy text holds those lines.
fn code_blocks(
    lang: &str,
    text: &str,
    style: BlockStyle,
    quote: i32,
    level: i32,
    out: &mut Vec<Block>,
) {
    let position = CodePosition {
        rows_before: 0,
        lines_before: 0,
        quote,
        level,
    };
    code_rows(lang, text, position, style, out);
}

/// The rows after the first `rows_before` rows (`lines_before` lines) of
/// a top-level code block whose remaining text is `text` (streaming).
pub(crate) fn code_blocks_after(
    lang: &str,
    text: &str,
    rows_before: usize,
    lines_before: usize,
    style: BlockStyle,
    out: &mut Vec<Block>,
) {
    let position = CodePosition {
        rows_before,
        lines_before,
        quote: 0,
        level: 0,
    };
    code_rows(lang, text, position, style, out);
}

/// Where the rows built by [`code_rows`] go.
#[derive(Clone, Copy)]
struct CodePosition {
    rows_before: usize,
    lines_before: usize,
    quote: i32,
    level: i32,
}

fn code_rows(lang: &str, text: &str, at: CodePosition, style: BlockStyle, out: &mut Vec<Block>) {
    let text = text.trim_end_matches(['\n', '\r']);
    let lines: Vec<&str> = text.lines().collect();
    let room = MAX_CODE_LINES.saturating_sub(at.lines_before);
    let shown = lines.len().min(room);
    let mut rows: Vec<String> = lines[..shown]
        .chunks(CODE_CHUNK_LINES)
        .map(|chunk| chunk.join("\n"))
        .collect();
    if rows.is_empty() && at.rows_before == 0 {
        rows.push(String::new());
    }
    let hidden = (shown < lines.len()).then(|| lines[shown..].join("\n"));
    if hidden.is_some() {
        rows.push(format!(
            "… {} more lines (use Copy for the full block)",
            lines.len() - shown
        ));
    }
    let count = at.rows_before + rows.len();
    for (offset, row) in rows.into_iter().enumerate() {
        let part = Part::of(at.rows_before + offset, count);
        out.push(code_row(lang, row, part, style, at.quote, at.level));
    }
    if let Some(hidden) = hidden
        && let Some(note) = out.last_mut()
    {
        note.copy_text = Some(hidden);
    }
}

/// One row of a code block.
pub(crate) fn code_row(
    lang: &str,
    text: String,
    part: Part,
    style: BlockStyle,
    quote: i32,
    level: i32,
) -> Block {
    let mut block = base_block(BlockKind::Code, style, quote, level);
    block.text = text;
    block.meta = lang.to_string();
    block.copyable = true;
    block.part = part;
    if matches!(part, Part::Middle | Part::Last) {
        block.gap = Gap::None;
    }
    block
}

fn is_simple_quote_child(node: &Node) -> bool {
    match node {
        Node::Paragraph(_) => true,
        Node::List { items, .. } => items.iter().all(is_simple_item),
        _ => false,
    }
}

fn is_simple_item(item: &ListItem) -> bool {
    item.children.iter().all(|child| match child {
        Node::Paragraph(_) => true,
        Node::List { items, .. } => items.iter().all(is_simple_item),
        _ => false,
    })
}

fn list_marker(start: Option<u64>, index: usize) -> String {
    match start {
        Some(start) => format!("{}. ", start + index as u64),
        None => "- ".to_string(),
    }
}

fn task_prefix(task: Option<bool>) -> &'static str {
    match task {
        Some(true) => "[x] ",
        Some(false) => "[ ] ",
        None => "",
    }
}

/// Re-serializes a simple list so `StyledText` can render it with markers.
fn list_markdown(start: Option<u64>, items: &[ListItem], indent: &str) -> String {
    items
        .iter()
        .enumerate()
        .map(|(index, item)| list_item_markdown(start, index, item, indent))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Rows of a simple list as `(depth, markdown)`: one per item, nested items
/// included. `StyledText` has no paragraph spacing, so separate rows give
/// list items room to breathe. Top-level items are Markdown list items (an
/// ordered one keeps its number, so a lone item renders as `3.`); nested
/// items get the marker `StyledText` would draw for them as text, since the
/// row's indentation shows the nesting.
fn list_rows(start: Option<u64>, items: &[ListItem], depth: usize, out: &mut Vec<(usize, String)>) {
    for (index, item) in items.iter().enumerate() {
        let marker = match (start, depth) {
            (_, 0) => list_marker(start, index),
            (Some(start), _) => escape_text(&format!("{}. ", start + index as u64)),
            (None, depth) if depth % 2 == 1 => "◦ ".to_string(),
            (None, _) => "• ".to_string(),
        };
        let child_indent = " ".repeat(if depth == 0 { marker.len() } else { 0 });
        let paragraphs: Vec<String> = item
            .children
            .iter()
            .filter_map(|child| match child {
                Node::Paragraph(inlines) => Some(emit_inlines(inlines, &child_indent)),
                _ => None,
            })
            .collect();
        let body = paragraphs.join(&format!("\n{child_indent}"));
        out.push((
            depth,
            format!("{marker}{}{body}", escape_text(task_prefix(item.task))),
        ));
        for child in &item.children {
            if let Node::List { start, items } = child {
                list_rows(*start, items, depth + 1, out);
            }
        }
    }
}

/// Item `index` of a simple list (with its nested lists) as Markdown.
fn list_item_markdown(start: Option<u64>, index: usize, item: &ListItem, indent: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let marker = list_marker(start, index);
    let child_indent = format!("{indent}{}", " ".repeat(marker.len()));
    let mut first = true;
    for child in &item.children {
        match child {
            Node::Paragraph(inlines) => {
                let body = emit_inlines(inlines, &child_indent);
                if first {
                    lines.push(format!(
                        "{indent}{marker}{}{body}",
                        escape_text(task_prefix(item.task))
                    ));
                    first = false;
                } else {
                    lines.push(format!("{child_indent}{body}"));
                }
            }
            Node::List { start, items } => {
                if first {
                    lines.push(format!(
                        "{indent}{marker}{}",
                        escape_text(task_prefix(item.task))
                    ));
                    first = false;
                }
                lines.push(list_markdown(*start, items, &child_indent));
            }
            _ => {}
        }
    }
    if first {
        lines.push(format!("{indent}{marker}"));
    }
    lines.join("\n")
}

/// Lists whose items contain code, quotes or tables: one row per child,
/// indented, with the marker on the first paragraph.
fn complex_list_blocks(
    start: Option<u64>,
    items: &[ListItem],
    style: BlockStyle,
    quote: i32,
    level: i32,
    out: &mut Vec<Block>,
) {
    let kind = if quote > 0 {
        BlockKind::Quote
    } else {
        BlockKind::Paragraph
    };
    for (index, item) in items.iter().enumerate() {
        let marker = escape_text(&format!(
            "{}{}",
            list_marker(start, index).replace("- ", "• "),
            task_prefix(item.task)
        ));
        let mut children = item.children.iter();
        match item.children.first() {
            Some(Node::Paragraph(inlines)) => {
                children.next();
                let mut block = base_block(kind, style, quote, level);
                block.rich = Some(format!("{marker}{}", emit_inlines(inlines, "")));
                out.push(block);
            }
            _ => {
                let mut block = base_block(kind, style, quote, level);
                block.rich = Some(marker);
                out.push(block);
            }
        }
        for child in children {
            node_blocks(child, style, quote, level + 1, out);
        }
    }
}

fn table(aligns: &[Alignment], rows: &[Vec<Vec<Inline>>]) -> Table {
    let columns = aligns
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0))
        .max(1);
    let mut weights = vec![MIN_COLUMN_WEIGHT; columns];
    let mut out_rows = Vec::with_capacity(rows.len());
    for (row_index, row) in rows.iter().enumerate() {
        let mut cells = Vec::with_capacity(columns);
        for (column, weight) in weights.iter_mut().enumerate() {
            let inlines = row.get(column).map(Vec::as_slice).unwrap_or_default();
            let chars = inline_plain(inlines).chars().count();
            *weight = (*weight).max(chars.min(MAX_COLUMN_WEIGHT));
            let markdown = if row_index == 0 {
                let inner = emit_inlines_with(inlines, "", /*in_strong*/ true);
                if inner.trim().is_empty() {
                    inner
                } else {
                    format!("**{inner}**")
                }
            } else {
                emit_inlines(inlines, "")
            };
            cells.push(markdown);
        }
        out_rows.push(cells);
    }
    let total: usize = weights.iter().sum();
    Table {
        rows: out_rows,
        widths: weights
            .iter()
            .map(|weight| *weight as f32 / total.max(1) as f32)
            .collect(),
        aligns: (0..columns)
            .map(|column| match aligns.get(column) {
                Some(Alignment::Center) => 1,
                Some(Alignment::Right) => 2,
                _ => 0,
            })
            .collect(),
    }
}

// ----- inline serialization -------------------------------------------------

/// Serializes inline content into markdown that `StyledText` accepts.
/// `indent` prefixes continuation lines (inside list items).
pub(crate) fn emit_inlines(inlines: &[Inline], indent: &str) -> String {
    emit_inlines_with(inlines, indent, /*in_strong*/ false)
}

fn emit_inlines_with(inlines: &[Inline], indent: &str, in_strong: bool) -> String {
    let mut emitter = Emitter {
        out: String::new(),
        indent,
        in_link: false,
        in_strong,
    };
    emitter.emit(inlines);
    emitter.out.trim_end().to_string()
}

struct Emitter<'a> {
    out: String,
    indent: &'a str,
    in_link: bool,
    in_strong: bool,
}

impl Emitter<'_> {
    fn emit(&mut self, inlines: &[Inline]) {
        for inline in inlines {
            match inline {
                Inline::Text(text) => self.text(text),
                Inline::Html(html) => self.out.push_str(&escape_text(html)),
                Inline::Code(code) => {
                    let span = code_span(code);
                    match citation_destination(code).filter(|_| !self.in_link) {
                        Some(dest) => {
                            self.out
                                .push_str(&format!("[{span}]({})", link_destination(&dest)));
                        }
                        None => self.out.push_str(&span),
                    }
                }
                Inline::Emphasis(children) => self.wrap("*", children),
                Inline::Strong(children) => {
                    if self.in_strong {
                        self.emit(children);
                    } else {
                        self.in_strong = true;
                        self.wrap("**", children);
                        self.in_strong = false;
                    }
                }
                Inline::Strikethrough(children) => self.wrap("~~", children),
                Inline::Link { dest, children } => {
                    if self.in_link || dest.trim().is_empty() {
                        self.emit(children);
                        continue;
                    }
                    self.in_link = true;
                    self.out.push('[');
                    if inline_plain(children).trim().is_empty() {
                        self.out.push_str(&escape_text(dest));
                    } else {
                        self.emit(children);
                    }
                    self.out.push_str("](");
                    self.out.push_str(&link_destination(dest));
                    self.out.push(')');
                    self.in_link = false;
                }
                Inline::Break => {
                    // Trailing spaces would turn into a hard break marker.
                    let trimmed = self.out.trim_end_matches(' ').len();
                    self.out.truncate(trimmed);
                    self.out.push('\n');
                    self.out.push_str(self.indent);
                }
            }
        }
    }

    fn wrap(&mut self, delimiter: &str, children: &[Inline]) {
        if inline_plain(children).trim().is_empty() {
            self.emit(children);
            return;
        }
        self.out.push_str(delimiter);
        self.emit(children);
        self.out.push_str(delimiter);
    }

    fn text(&mut self, text: &str) {
        if self.in_link {
            self.out.push_str(&escape_text(text));
            return;
        }
        let mut offset = 0;
        for (range, dest) in autolink_ranges(text) {
            self.out.push_str(&escape_text(&text[offset..range.start]));
            let label = &text[range.clone()];
            self.out.push_str(&format!(
                "[{}]({})",
                escape_text(label),
                link_destination(&dest)
            ));
            offset = range.end;
        }
        self.out.push_str(&escape_text(&text[offset..]));
    }
}

/// Backslash-escapes every character that could start markdown structure.
pub(crate) fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    for ch in text.chars() {
        match ch {
            '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '(' | ')' | '#' | '+' | '-' | '.'
            | '!' | '<' | '>' | '|' | '~' | '=' | '&' => {
                out.push('\\');
                out.push(ch);
            }
            '\t' => out.push_str("    "),
            '\r' => {}
            _ => out.push(ch),
        }
    }
    out
}

/// Inline code span with a fence longer than any backtick run inside.
pub(crate) fn code_span(code: &str) -> String {
    let longest = code.split(|ch| ch != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    let pad = code.starts_with('`') || code.ends_with('`') || code.starts_with(' ');
    if pad {
        format!("{fence} {code} {fence}")
    } else {
        format!("{fence}{code}{fence}")
    }
}

/// `dest` as a CommonMark link destination (`<…>`) that parses back to
/// exactly `dest`: backslashes are escaped (CommonMark unescapes `\` before
/// punctuation inside destinations, so `C:\repo\.github` would lose one),
/// and so are the angle brackets. Line breaks cannot be represented and are
/// dropped.
pub(crate) fn link_destination(dest: &str) -> String {
    let mut out = String::with_capacity(dest.len() + 2);
    out.push('<');
    for ch in dest.chars() {
        match ch {
            '\n' | '\r' => {}
            '\\' | '<' | '>' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out.push('>');
    out
}

/// Plain text of inline content (for widths, copy, and fallbacks).
pub(crate) fn inline_plain(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(text) | Inline::Code(text) | Inline::Html(text) => out.push_str(text),
            Inline::Emphasis(children)
            | Inline::Strong(children)
            | Inline::Strikethrough(children)
            | Inline::Link { children, .. } => out.push_str(&inline_plain(children)),
            Inline::Break => out.push('\n'),
        }
    }
    out
}

/// Plain text of a (sanitized) markdown string.
pub(crate) fn markdown_to_plain(markdown: &str) -> String {
    let mut out = String::new();
    for event in Parser::new_ext(markdown, parser_options()) {
        match event {
            Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text) => {
                out.push_str(&text);
            }
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            Event::End(pulldown_cmark::TagEnd::Paragraph | pulldown_cmark::TagEnd::Item) => {
                out.push('\n');
            }
            _ => {}
        }
    }
    out.trim_end().to_string()
}

// ----- fences and tables (line level) ------------------------------------

/// Tracks whether successive source lines are inside a fenced code block.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct FenceTracker {
    open: Option<(char, usize)>,
}

impl FenceTracker {
    pub(crate) fn in_fence(&self) -> bool {
        self.open.is_some()
    }

    /// Feeds one complete line (without the newline).
    pub(crate) fn advance(&mut self, line: &str) {
        let Some((ch, len, rest)) = fence_marker(line) else {
            return;
        };
        match self.open {
            None => self.open = Some((ch, len)),
            Some((open_ch, open_len)) => {
                if ch == open_ch && len >= open_len && rest.trim().is_empty() {
                    self.open = None;
                }
            }
        }
    }
}

/// `(char, run length, info)` when `line` starts a code fence.
pub(crate) fn fence_marker(line: &str) -> Option<(char, usize, &str)> {
    let line = strip_quote_prefix(line);
    let spaces = line.len() - line.trim_start_matches(' ').len();
    if spaces > 3 {
        return None;
    }
    let line = &line[spaces..];
    let ch = line.chars().next().filter(|ch| *ch == '`' || *ch == '~')?;
    let len = line.chars().take_while(|c| *c == ch).count();
    if len < 3 {
        return None;
    }
    let rest = &line[len..];
    if ch == '`' && rest.contains('`') {
        return None;
    }
    Some((ch, len, rest))
}

fn strip_quote_prefix(line: &str) -> &str {
    let mut rest = line;
    loop {
        let trimmed = rest.trim_start_matches(' ');
        match trimmed.strip_prefix('>') {
            Some(after) => rest = after.strip_prefix(' ').unwrap_or(after),
            None => return rest,
        }
    }
}

/// Pipe-separated cells of a possible table line.
pub(crate) fn table_segments(line: &str) -> Option<Vec<String>> {
    let line = strip_quote_prefix(line).trim();
    if line.is_empty() {
        return None;
    }
    let outer = line.starts_with('|') || line.ends_with('|');
    let inner = line.strip_prefix('|').unwrap_or(line);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&'|') => {
                current.push('|');
                chars.next();
            }
            '|' => segments.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    segments.push(current);
    (outer || segments.len() >= 2).then_some(segments)
}

pub(crate) fn is_table_delimiter_line(line: &str) -> bool {
    table_segments(line).is_some_and(|segments| {
        segments.iter().all(|segment| {
            let segment = segment.trim();
            let core = segment.strip_prefix(':').unwrap_or(segment);
            let core = core.strip_suffix(':').unwrap_or(core);
            core.len() >= 3 && core.chars().all(|ch| ch == '-')
        })
    })
}

pub(crate) fn is_table_header_line(line: &str) -> bool {
    table_segments(line).is_some_and(|segments| segments.iter().any(|s| !s.trim().is_empty()))
}

/// Drops the fences around ```` ```md ```` blocks that contain a table, so
/// the table renders (models often wrap tables that way). Other fences and
/// unclosed blocks are kept verbatim.
pub(crate) fn unwrap_markdown_fences(source: &str) -> Cow<'_, str> {
    if !source.contains("```") && !source.contains("~~~") {
        return Cow::Borrowed(source);
    }
    let mut out = String::with_capacity(source.len());
    let mut changed = false;
    let mut lines = source.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        let content = line.trim_end_matches(['\n', '\r']);
        let Some((ch, len, info)) = fence_marker(content) else {
            out.push_str(line);
            continue;
        };
        let is_markdown = matches!(
            info.split_whitespace()
                .next()
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("md" | "markdown")
        );
        let mut body: Vec<&str> = Vec::new();
        let mut closing: Option<&str> = None;
        for next in lines.by_ref() {
            let next_content = next.trim_end_matches(['\n', '\r']);
            if let Some((next_ch, next_len, rest)) = fence_marker(next_content)
                && next_ch == ch
                && next_len >= len
                && rest.trim().is_empty()
            {
                closing = Some(next);
                break;
            }
            body.push(next);
        }
        let has_table = body.windows(2).any(|pair| {
            is_table_header_line(pair[0].trim_end()) && is_table_delimiter_line(pair[1].trim_end())
        });
        if is_markdown && closing.is_some() && has_table {
            changed = true;
            for body_line in body {
                out.push_str(body_line);
            }
        } else {
            out.push_str(line);
            for body_line in body {
                out.push_str(body_line);
            }
            if let Some(closing) = closing {
                out.push_str(closing);
            }
        }
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn kinds(blocks: &[Block]) -> Vec<BlockKind> {
        blocks.iter().map(|block| block.kind).collect()
    }

    fn rich(block: &Block) -> &str {
        block.rich.as_deref().unwrap_or_default()
    }

    #[test]
    fn splits_block_structure_into_rows() {
        let source = "# Title\n\nSome **bold** text.\n\n```rust\nfn main() {}\n```\n\n> quoted\n\n---\n\n| a | b |\n|---|--:|\n| 1 | 2 |\n";
        let blocks = render(source, BlockStyle::default());
        assert_eq!(
            kinds(&blocks),
            vec![
                BlockKind::Heading,
                BlockKind::Paragraph,
                BlockKind::Code,
                BlockKind::Quote,
                BlockKind::Rule,
                BlockKind::Table,
            ]
        );
        assert_eq!(rich(&blocks[0]), "**Title**");
        assert_eq!(blocks[0].level, 1);
        assert_eq!(rich(&blocks[1]), "Some **bold** text\\.");
        assert_eq!(blocks[2].text, "fn main() {}");
        assert_eq!(blocks[2].meta, "rust");
        assert_eq!(blocks[3].quote, 1);
        let table = blocks[5].table.as_ref().map(|table| table.aligns.clone());
        assert_eq!(table, Some(vec![0, 2]));
    }

    #[test]
    fn long_code_blocks_are_split_into_rows() {
        let code: String = (0..150).map(|index| format!("line {index}\n")).collect();
        let blocks = render(&format!("```rust\n{code}```\n"), BlockStyle::default());
        let rows: Vec<(BlockKind, Part, Gap, usize)> = blocks
            .iter()
            .map(|block| {
                (
                    block.kind,
                    block.part,
                    block.gap,
                    block.text.lines().count(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (BlockKind::Code, Part::First, Gap::Block, CODE_CHUNK_LINES),
                (BlockKind::Code, Part::Middle, Gap::None, CODE_CHUNK_LINES),
                (
                    BlockKind::Code,
                    Part::Last,
                    Gap::None,
                    150 - 2 * CODE_CHUNK_LINES
                ),
            ]
        );
        assert!(blocks.iter().all(|block| block.meta == "rust"));
        // Short and empty blocks stay one row.
        let short = render("```\nx\n```\n", BlockStyle::default());
        assert_eq!(short.len(), 1);
        assert_eq!(short[0].part, Part::Whole);
        let empty = render("```\n```\n", BlockStyle::default());
        assert_eq!((empty.len(), empty[0].text.as_str()), (1, ""));
    }

    #[test]
    fn cut_code_blocks_end_with_a_note_holding_the_rest() {
        let code: String = (0..MAX_CODE_LINES + 5)
            .map(|index| format!("{index}\n"))
            .collect();
        let blocks = render(&format!("```\n{code}```\n"), BlockStyle::default());
        let note = blocks
            .last()
            .map(|block| (block.text.clone(), block.copy_text.clone()));
        assert_eq!(
            note,
            Some((
                "… 5 more lines (use Copy for the full block)".to_string(),
                Some("2000\n2001\n2002\n2003\n2004".to_string())
            ))
        );
        let shown: usize = blocks[..blocks.len() - 1]
            .iter()
            .map(|block| block.text.lines().count())
            .sum();
        assert_eq!(shown, MAX_CODE_LINES);
    }

    #[test]
    fn inline_html_and_generics_are_escaped() {
        let blocks = render("Use Vec<String> and <b>x</b> here", BlockStyle::default());
        assert_eq!(
            rich(&blocks[0]),
            "Use Vec\\<String\\> and \\<b\\>x\\</b\\> here"
        );
        assert!(slint::StyledText::from_markdown(rich(&blocks[0])).is_ok());
    }

    #[test]
    fn soft_breaks_stay_line_breaks() {
        let blocks = render("line one\nline two", BlockStyle::default());
        assert_eq!(rich(&blocks[0]), "line one\nline two");
    }

    #[test]
    fn simple_lists_render_one_row_per_item() {
        let source =
            "1. Read `src/main.rs`\n2. Update\n   - handle `Vec<String>`\n   - [x] done\n3. Test\n";
        let blocks = render(source, BlockStyle::default());
        let rows: Vec<(i32, Gap, &str)> = blocks
            .iter()
            .map(|block| (block.level, block.gap, rich(block)))
            .collect();
        assert_eq!(
            rows,
            vec![
                (0, Gap::Block, "1. Read [`src/main.rs`](<src/main.rs>)"),
                (0, Gap::Item, "2. Update"),
                (1, Gap::Item, "◦ handle `Vec<String>`"),
                (1, Gap::Item, "◦ \\[x\\] done"),
                // A lone ordered item keeps its number.
                (0, Gap::Item, "3. Test"),
            ]
        );
        for block in &blocks {
            assert_eq!(block.kind, BlockKind::Paragraph);
            assert!(slint::StyledText::from_markdown(rich(block)).is_ok());
        }
        let nested = render("1. a\n   1. b\n      - c\n", BlockStyle::default());
        let nested: Vec<(i32, &str)> = nested
            .iter()
            .map(|block| (block.level, rich(block)))
            .collect();
        assert_eq!(nested, vec![(0, "1. a"), (1, "1\\. b"), (2, "• c")]);
    }

    #[test]
    fn lists_with_code_are_split() {
        let source = "- step\n\n  ```sh\n  ls\n  ```\n- next\n";
        let blocks = render(source, BlockStyle::default());
        assert_eq!(
            kinds(&blocks),
            vec![BlockKind::Paragraph, BlockKind::Code, BlockKind::Paragraph]
        );
        assert_eq!(rich(&blocks[0]), "• step");
        assert_eq!(blocks[1].level, 1);
        assert_eq!(rich(&blocks[2]), "• next");
    }

    #[test]
    fn links_and_bare_urls_become_links() {
        let blocks = render(
            "See [docs](https://example.com/a) or https://example.org/x.",
            BlockStyle::default(),
        );
        assert_eq!(
            rich(&blocks[0]),
            "See [docs](<https://example.com/a>) or [https://example\\.org/x](<https://example.org/x>)\\."
        );
    }

    #[test]
    fn images_render_alt_text() {
        let blocks = render("![diagram](x.png) done", BlockStyle::default());
        assert_eq!(rich(&blocks[0]), "diagram done");
    }

    /// Link destinations of `markdown`, as a CommonMark parser reads them.
    fn parsed_destinations(markdown: &str) -> Vec<String> {
        Parser::new_ext(markdown, parser_options())
            .filter_map(|event| match event {
                Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.into_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn link_destinations_round_trip() {
        for dest in [
            r"C:\proj\.github\ci.yml:3",
            r"C:\repo\_build\x.rs",
            r"\\server\share\a b.rs",
            "a<b>c.rs",
            "https://example.com/a_b?x=(1)",
        ] {
            let markdown = format!("[x]({})", link_destination(dest));
            assert_eq!(parsed_destinations(&markdown), vec![dest.to_string()]);
            assert!(slint::StyledText::from_markdown(&markdown).is_ok());
        }
        // Citations in code spans keep their backslashes through rendering.
        let blocks = render(r"see `C:\repo\.github\ci.yml:12`", BlockStyle::default());
        assert_eq!(
            parsed_destinations(rich(&blocks[0])),
            vec![r"C:\repo\.github\ci.yml:12".to_string()]
        );
    }

    #[test]
    fn code_spans_with_backticks_use_longer_fences() {
        assert_eq!(code_span("a`b"), "``a`b``");
        assert_eq!(code_span("`x"), "`` `x ``");
    }

    #[test]
    fn markdown_fences_with_tables_are_unwrapped() {
        let source = "```md\n| a | b |\n|---|---|\n| 1 | 2 |\n```\n";
        assert_eq!(
            unwrap_markdown_fences(source),
            "| a | b |\n|---|---|\n| 1 | 2 |\n"
        );
        let plain = "```md\njust text\n```\n";
        assert_eq!(unwrap_markdown_fences(plain), plain);
        let blocks = render(source, BlockStyle::default());
        assert_eq!(kinds(&blocks), vec![BlockKind::Table]);
    }

    #[test]
    fn offsets_map_back_through_unwrapped_fences() {
        let source = "```md\n| a | b |\n|---|---|\n```\n\nafter\n";
        let parsed = parse(source);
        let offsets: Vec<Option<usize>> = parsed.nodes.iter().map(|(start, _)| *start).collect();
        assert_eq!(offsets.len(), 2);
        assert_eq!(offsets[1], Some(source.find("after").unwrap_or_default()));
        assert_eq!(offsets[0], None);
    }

    #[test]
    fn detects_reference_definitions() {
        assert!(parse("[x]: https://a.b\n\n[x]").has_reference_definitions);
        assert!(!parse("plain").has_reference_definitions);
    }

    #[test]
    fn fence_tracker_opens_and_closes() {
        let mut tracker = FenceTracker::default();
        tracker.advance("```rust");
        assert!(tracker.in_fence());
        tracker.advance("``");
        assert!(tracker.in_fence());
        tracker.advance("````");
        assert!(!tracker.in_fence());
        tracker.advance("~~~");
        tracker.advance("```");
        assert!(tracker.in_fence());
    }

    #[test]
    fn table_line_detection() {
        assert!(is_table_header_line("| a | b |"));
        assert!(is_table_delimiter_line("|---|:---:|"));
        assert!(!is_table_delimiter_line("|--|"));
        assert!(!is_table_delimiter_line("| a | b |"));
        assert!(!is_table_header_line("plain text"));
    }

    #[test]
    fn markdown_to_plain_strips_formatting() {
        assert_eq!(markdown_to_plain("**a** \\<b\\>\n- c"), "a <b>\nc");
    }

    #[test]
    fn sanitized_output_always_parses() {
        let samples = [
            "*a* _b_ __c__ ~~d~~ `e` [f](g h) <https://x.y>",
            "1. x\n2) y\n\n+ z\n\n* w",
            "Text with | pipes | and # hashes and 2*3*4",
            "Unclosed **bold and `code",
            "Footnote[^1] and <div>html</div>",
        ];
        for sample in samples {
            for block in render(sample, BlockStyle::default()) {
                if let Some(markdown) = &block.rich {
                    assert!(
                        slint::StyledText::from_markdown(markdown).is_ok(),
                        "{sample:?} produced {markdown:?}"
                    );
                }
            }
        }
    }
}
