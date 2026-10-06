//! Rust-side description of one transcript row and its conversion into the
//! Slint `BlockData` struct.
//!
//! Every visible row of the transcript is a [`Block`]. Blocks are plain data
//! (cheap to compare, easy to test); [`Block::to_row`] turns one into the
//! generated `BlockData`, parsing inline markdown into `StyledText` only at
//! that point. The numeric codes below are mirrored in `ui/transcript.slint`.

use std::rc::Rc;

use slint::ModelRc;
use slint::SharedString;
use slint::StyledText;
use slint::VecModel;

use crate::ui::BlockData;
use crate::ui::BlockLine;

/// Row layout selector (`BlockData.kind`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum BlockKind {
    /// Inline markdown paragraph or simple list.
    #[default]
    Paragraph = 0,
    Heading = 1,
    /// Fenced or indented code; selectable monospace text.
    Code = 2,
    Table = 3,
    /// Paragraph inside a block quote.
    Quote = 4,
    Rule = 5,
    /// A user message bubble with attachment chips.
    User = 6,
    /// Collapsible reasoning summary.
    Reasoning = 7,
    /// Shell command with status and collapsible output.
    Exec = 8,
    /// Compact read/list/search command ("Explored").
    Explore = 9,
    /// Summary row of a multi-file patch.
    PatchSummary = 10,
    /// One file of a patch with an expandable diff.
    PatchFile = 11,
    /// MCP / dynamic tool / web search / image card.
    Tool = 12,
    /// Sub-agent card with clickable agents.
    Agent = 13,
    /// Info, warning, or error row.
    Notice = 14,
    /// Thin divider with a caption ("Worked for 12s").
    Separator = 15,
    /// Plan checklist (`turn/plan/updated`).
    PlanUpdate = 16,
    /// Header above a group of blocks ("Proposed plan", "Code review").
    Section = 17,
    /// Title row of a framed card (recaps, cross-tab messages).
    CardHeader = 18,
}

/// Lifecycle shown by status glyphs (`BlockData.status`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Status {
    #[default]
    None = 0,
    Running = 1,
    Ok = 2,
    Failed = 3,
    Declined = 4,
    Interrupted = 5,
}

/// Color scheme of a row (`BlockData.tone`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Tone {
    #[default]
    Normal = 0,
    Muted = 1,
    Plan = 2,
    Info = 3,
    Warning = 4,
    Error = 5,
}

/// Vertical space above a row (`BlockData.gap`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Gap {
    /// Directly attached to the previous row (rows of one card).
    None = 0,
    /// Between blocks of one message.
    #[default]
    Block = 1,
    /// Between transcript entries.
    Entry = 2,
    /// Between the items of a list (tighter than between blocks).
    Item = 3,
}

/// Position of a row inside a framed card (`BlockData.frame`). Consecutive
/// rows of one entry draw one box: the top row its top edge, the bottom row
/// its bottom edge, and the rows between only the sides.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Frame {
    #[default]
    None = 0,
    Top = 1,
    Middle = 2,
    Bottom = 3,
    /// A card of a single row.
    Single = 4,
}

/// Position of a code row in a code block split into several rows
/// (`BlockData.part`): the first row has the header and top edge, the last
/// row the bottom edge, and they draw one box together.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Part {
    /// The whole block in one row.
    #[default]
    Whole = 0,
    First = 1,
    Middle = 2,
    Last = 3,
}

impl Part {
    /// Part of row `index` of `count` rows.
    pub(crate) fn of(index: usize, count: usize) -> Self {
        match (index == 0, index + 1 >= count) {
            (true, true) => Self::Whole,
            (true, false) => Self::First,
            (false, true) => Self::Last,
            (false, false) => Self::Middle,
        }
    }
}

/// Marks `blocks` as the rows of one framed card.
pub(crate) fn frame_blocks(blocks: &mut [Block]) {
    let count = blocks.len();
    for (index, block) in blocks.iter_mut().enumerate() {
        block.frame = match (index == 0, index + 1 == count) {
            (true, true) => Frame::Single,
            (true, false) => Frame::Top,
            (false, true) => Frame::Bottom,
            (false, false) => Frame::Middle,
        };
    }
}

/// Kinds of [`Line`] inside diff views (`BlockLine.kind`).
pub(crate) mod line_kind {
    pub(crate) const CONTEXT: i32 = 0;
    pub(crate) const ADDED: i32 = 1;
    pub(crate) const REMOVED: i32 = 2;
    /// Hunk separator.
    pub(crate) const GAP: i32 = 3;
    /// "… N more lines" note.
    pub(crate) const NOTE: i32 = 4;
}

/// A sub-line of a block: diff line, plan step, agent, chip, or hook entry.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Line {
    pub(crate) text: String,
    pub(crate) kind: i32,
    pub(crate) gutter: String,
    /// Click target (thread id, path); empty when not clickable.
    pub(crate) target: String,
}

impl Line {
    pub(crate) fn new(text: impl Into<String>, kind: i32) -> Self {
        Self {
            text: text.into(),
            kind,
            gutter: String::new(),
            target: String::new(),
        }
    }
}

/// A markdown table: inline-markdown cells, row-major, header first.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Table {
    pub(crate) rows: Vec<Vec<String>>,
    /// Column width fractions summing to 1.
    pub(crate) widths: Vec<f32>,
    /// 0 = left, 1 = center, 2 = right.
    pub(crate) aligns: Vec<i32>,
}

/// One transcript row.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Block {
    pub(crate) kind: BlockKind,
    pub(crate) title: String,
    /// Plain or monospace body text.
    pub(crate) text: String,
    /// Inline markdown restricted to what `StyledText` supports (see
    /// [`crate::transcript::markdown`]); rendered rich when `Some`.
    pub(crate) rich: Option<String>,
    pub(crate) status: Status,
    pub(crate) tone: Tone,
    pub(crate) gap: Gap,
    pub(crate) frame: Frame,
    /// Code rows: where the row sits in its code block.
    pub(crate) part: Part,
    /// Heading level or list indentation.
    pub(crate) level: i32,
    /// Block quote depth.
    pub(crate) quote: i32,
    pub(crate) meta: String,
    pub(crate) detail: String,
    pub(crate) target: String,
    pub(crate) added: i32,
    pub(crate) removed: i32,
    pub(crate) lines: Vec<Line>,
    pub(crate) table: Option<Table>,
    /// Identifies the expandable element of the owning entry this row
    /// controls; `None` when the row cannot be toggled.
    pub(crate) toggle: Option<usize>,
    pub(crate) expanded: bool,
    /// Show a copy affordance (code, output).
    pub(crate) copyable: bool,
    /// Text copied by the row's Copy action when it differs from `text`
    /// (for example the full output when only a tail is displayed).
    pub(crate) copy_text: Option<String>,
    /// The row belongs to a message that supports message-level actions
    /// (copy as markdown, quote, forward).
    pub(crate) message: bool,
    /// Last row of a completed message: shows the message actions bar.
    pub(crate) footer: bool,
}

impl Block {
    pub(crate) fn new(kind: BlockKind) -> Self {
        Self {
            kind,
            ..Self::default()
        }
    }

    /// Rough byte size of the text this row keeps alive (for memory caps).
    pub(crate) fn byte_len(&self) -> usize {
        let lines: usize = self
            .lines
            .iter()
            .map(|line| line.text.len() + line.gutter.len() + line.target.len())
            .sum();
        let table: usize = self
            .table
            .as_ref()
            .map(|table| table.rows.iter().flatten().map(String::len).sum())
            .unwrap_or(0);
        // Rich text is stored twice: the source and the styled copy.
        self.title.len()
            + self.text.len()
            + self.rich.as_ref().map_or(0, |rich| rich.len() * 2)
            + self.meta.len()
            + self.detail.len()
            + self.target.len()
            + self.copy_text.as_ref().map_or(0, String::len)
            + lines
            + table
            + 64
    }

    /// Converts to the generated Slint row. `id` must be unique in the model.
    pub(crate) fn to_row(&self, id: &str) -> BlockData {
        let (rich, use_rich) = match &self.rich {
            Some(markdown) => (styled_text(markdown), true),
            None => (StyledText::default(), false),
        };
        let lines: Vec<BlockLine> = self
            .lines
            .iter()
            .map(|line| BlockLine {
                text: line.text.as_str().into(),
                kind: line.kind,
                gutter: line.gutter.as_str().into(),
                target: line.target.as_str().into(),
            })
            .collect();
        let (cells, widths, aligns) = match &self.table {
            Some(table) => {
                let rows: Vec<ModelRc<StyledText>> = table
                    .rows
                    .iter()
                    .map(|row| {
                        let cells: Vec<StyledText> =
                            row.iter().map(|cell| styled_text(cell)).collect();
                        ModelRc::from(Rc::new(VecModel::from(cells)))
                    })
                    .collect();
                (
                    ModelRc::from(Rc::new(VecModel::from(rows))),
                    ModelRc::from(Rc::new(VecModel::from(table.widths.clone()))),
                    ModelRc::from(Rc::new(VecModel::from(table.aligns.clone()))),
                )
            }
            None => (ModelRc::default(), ModelRc::default(), ModelRc::default()),
        };
        BlockData {
            id: id.into(),
            kind: self.kind as i32,
            title: SharedString::from(self.title.as_str()),
            text: SharedString::from(self.text.as_str()),
            rich,
            use_rich,
            status: self.status as i32,
            expanded: self.expanded,
            expandable: self.toggle.is_some(),
            tone: self.tone as i32,
            gap: self.gap as i32,
            frame: self.frame as i32,
            part: self.part as i32,
            level: self.level,
            quote: self.quote,
            meta: self.meta.as_str().into(),
            detail: self.detail.as_str().into(),
            target: self.target.as_str().into(),
            added: self.added,
            removed: self.removed,
            lines: if lines.is_empty() {
                ModelRc::default()
            } else {
                ModelRc::from(Rc::new(VecModel::from(lines)))
            },
            cells,
            widths,
            aligns,
            copyable: self.copyable,
            message: self.message,
            footer: self.footer,
        }
    }
}

/// Parses sanitized inline markdown, falling back to plain text when Slint's
/// markdown subset rejects it.
pub(crate) fn styled_text(markdown: &str) -> StyledText {
    StyledText::from_markdown(markdown).unwrap_or_else(|err| {
        tracing::debug!(%err, "falling back to plain text for a transcript row");
        StyledText::from_plain_text(&crate::transcript::markdown::markdown_to_plain(markdown))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use slint::Model;

    #[test]
    fn converts_lines_and_flags() {
        let mut block = Block::new(BlockKind::PatchFile);
        block.title = "src/lib.rs".to_string();
        block.lines = vec![Line::new("+added", line_kind::ADDED)];
        block.toggle = Some(2);
        block.expanded = true;
        let row = block.to_row("item#0");
        assert_eq!(row.id.as_str(), "item#0");
        assert_eq!(row.kind, BlockKind::PatchFile as i32);
        assert!(row.expandable);
        assert!(row.expanded);
        assert_eq!(row.lines.row_count(), 1);
        assert_eq!(
            row.lines.row_data(0).map(|line| line.kind),
            Some(line_kind::ADDED)
        );
        assert!(!row.use_rich);
    }

    #[test]
    fn frames_mark_first_middle_and_last_rows() {
        let mut blocks = vec![Block::new(BlockKind::CardHeader); 3];
        frame_blocks(&mut blocks);
        let frames: Vec<Frame> = blocks.iter().map(|block| block.frame).collect();
        assert_eq!(frames, vec![Frame::Top, Frame::Middle, Frame::Bottom]);
        let mut single = vec![Block::new(BlockKind::CardHeader)];
        frame_blocks(&mut single);
        assert_eq!(single[0].frame, Frame::Single);
        assert_eq!(single[0].to_row("c#0").frame, Frame::Single as i32);
    }

    #[test]
    fn invalid_markdown_falls_back_to_plain_text() {
        let mut block = Block::new(BlockKind::Paragraph);
        block.rich = Some("# not inline".to_string());
        let row = block.to_row("x#0");
        assert!(row.use_rich);
        assert_ne!(row.rich, StyledText::default());
    }

    #[test]
    fn tables_convert_to_nested_models() {
        let mut block = Block::new(BlockKind::Table);
        block.table = Some(Table {
            rows: vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["1".to_string(), "2".to_string()],
            ],
            widths: vec![0.5, 0.5],
            aligns: vec![0, 2],
        });
        let row = block.to_row("t#0");
        assert_eq!(row.cells.row_count(), 2);
        assert_eq!(
            row.cells.row_data(1).map(|cells| cells.row_count()),
            Some(2)
        );
        assert_eq!(row.aligns.row_data(1), Some(2));
    }
}
