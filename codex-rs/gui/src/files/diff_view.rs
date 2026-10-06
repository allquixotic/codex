//! Diff tabs: parsing off the UI thread when large, a lazily converted row
//! model with per-file collapsing, and the diff actions.

use std::cell::RefCell;
use std::collections::HashSet;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use codex_protocol::num_format::format_with_separators;
use slint::ComponentHandle;
use slint::Model;
use slint::ModelNotify;
use slint::ModelRc;
use slint::ModelTracker;
use slint::SharedString;
use slint::VecModel;

use super::diff;
use super::diff::DiffLine;
use super::diff::LineKind;
use super::diff::ParsedDiff;
use super::diff_doc_at;
use super::tab_key;
use crate::app::AppController;
use crate::app::TabId;
use crate::ui::DiffRowData;
use crate::ui::DiffSpan;
use crate::ui::FilesState;

/// Diffs up to this size are parsed on the UI thread (no "Reading…" flash).
const SYNC_PARSE_BYTES: usize = 128 * 1024;
const TAB_WIDTH: usize = 4;

/// Row kinds shared with `DiffKinds` in ui/files_diff.slint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DiffRowKind {
    FileHeader = 0,
    Meta = 1,
    Hunk = 2,
    Context = 3,
    Added = 4,
    Removed = 5,
    NoNewline = 6,
    Binary = 7,
}

impl From<LineKind> for DiffRowKind {
    fn from(kind: LineKind) -> Self {
        match kind {
            LineKind::FileHeader => Self::FileHeader,
            LineKind::Meta => Self::Meta,
            LineKind::Hunk => Self::Hunk,
            LineKind::Context => Self::Context,
            LineKind::Added => Self::Added,
            LineKind::Removed => Self::Removed,
            LineKind::NoNewline => Self::NoNewline,
            LineKind::Binary => Self::Binary,
        }
    }
}

/// State of one diff tab.
pub(super) struct DiffDoc {
    pub(super) title: String,
    /// Folder the diff's relative paths are resolved against.
    pub(super) base: Option<PathBuf>,
    raw: String,
    model: Option<Rc<DiffRowsModel>>,
    generation: u64,
    pub(super) scroll: (f32, f32),
}

impl DiffDoc {
    pub(super) fn new(title: String, raw: String, base: Option<PathBuf>) -> Self {
        Self {
            title,
            base,
            raw,
            model: None,
            generation: 0,
            scroll: (0.0, 0.0),
        }
    }

    /// Replaces the diff text (the previous rows stay until it is parsed).
    pub(super) fn replace(&mut self, raw: String, base: Option<PathBuf>) {
        self.raw = raw;
        if base.is_some() {
            self.base = base;
        }
    }
}

/// Rows of one parsed diff; collapsed files hide their lines.
pub(super) struct DiffRowsModel {
    diff: ParsedDiff,
    /// Lines (header first) belonging to each file.
    file_lines: Vec<Range<usize>>,
    /// Where "Open" leads for each file, with the first changed line.
    open_targets: Vec<Option<(PathBuf, Option<usize>)>>,
    collapsed: RefCell<Vec<bool>>,
    /// Indices into `diff.lines` of the visible rows.
    visible: RefCell<Vec<usize>>,
    max_chars: usize,
    notify: ModelNotify,
}

impl DiffRowsModel {
    fn new(diff: ParsedDiff, base: Option<&Path>, collapsed_paths: &HashSet<String>) -> Self {
        let mut file_lines = vec![0..0; diff.files.len()];
        for (index, line) in diff.lines.iter().enumerate() {
            if let Some(range) = line.file.and_then(|file| file_lines.get_mut(file)) {
                if range.end == 0 {
                    range.start = index;
                }
                range.end = index + 1;
            }
        }
        let open_targets = diff
            .files
            .iter()
            .enumerate()
            .map(|(file, entry)| {
                let path = resolve_path(entry.current_path()?, base)?;
                let line = diff.lines[file_lines[file].clone()]
                    .iter()
                    .find(|line| line.kind == LineKind::Added)
                    .or_else(|| {
                        diff.lines[file_lines[file].clone()]
                            .iter()
                            .find(|line| line.new_no.is_some())
                    })
                    .and_then(|line| line.new_no)
                    .and_then(|line| usize::try_from(line).ok());
                Some((path, line))
            })
            .collect();
        let collapsed: Vec<bool> = diff
            .files
            .iter()
            .map(|file| collapsed_paths.contains(&file.display_path()))
            .collect();
        let max_chars = diff
            .lines
            .iter()
            .map(|line| expanded_width(&line.text))
            .max()
            .unwrap_or(0);
        let model = Self {
            diff,
            file_lines,
            open_targets,
            collapsed: RefCell::new(collapsed),
            visible: RefCell::new(Vec::new()),
            max_chars,
            notify: ModelNotify::default(),
        };
        model.rebuild_visible();
        model
    }

    fn rebuild_visible(&self) {
        let collapsed = self.collapsed.borrow();
        let visible = visible_rows(&self.diff.lines, &collapsed);
        *self.visible.borrow_mut() = visible;
    }

    fn collapsed_paths(&self) -> HashSet<String> {
        let collapsed = self.collapsed.borrow();
        self.diff
            .files
            .iter()
            .zip(collapsed.iter())
            .filter(|(_, collapsed)| **collapsed)
            .map(|(file, _)| file.display_path())
            .collect()
    }

    /// Collapses or expands one file, notifying only the affected rows.
    fn toggle(&self, file: usize) {
        let Some(lines) = self.file_lines.get(file).cloned() else {
            return;
        };
        let collapse = {
            let mut collapsed = self.collapsed.borrow_mut();
            let Some(flag) = collapsed.get_mut(file) else {
                return;
            };
            *flag = !*flag;
            *flag
        };
        let header = lines.start;
        let change = {
            let mut visible = self.visible.borrow_mut();
            let Some(position) = visible.iter().position(|&line| line == header) else {
                return;
            };
            if collapse {
                let count = visible[position + 1..]
                    .iter()
                    .take_while(|&&line| self.diff.lines[line].file == Some(file))
                    .count();
                visible.drain(position + 1..position + 1 + count);
                (position, count, false)
            } else {
                let rows: Vec<usize> = (header + 1..lines.end).collect();
                let count = rows.len();
                visible.splice(position + 1..position + 1, rows);
                (position, count, true)
            }
        };
        let (position, count, added) = change;
        self.notify.row_changed(position);
        if count > 0 {
            if added {
                self.notify.row_added(position + 1, count);
            } else {
                self.notify.row_removed(position + 1, count);
            }
        }
    }

    fn set_all_collapsed(&self, collapsed: bool) {
        self.collapsed
            .borrow_mut()
            .iter_mut()
            .for_each(|flag| *flag = collapsed);
        self.rebuild_visible();
        self.notify.reset();
    }

    fn summary(&self) -> String {
        let files = self.diff.files.len();
        if files == 0 {
            return String::new();
        }
        let (added, removed) = self.diff.totals();
        let noun = if files == 1 { "file" } else { "files" };
        format!(
            "{files} {noun} changed · +{} −{}",
            format_with_separators(i64::try_from(added).unwrap_or(i64::MAX)),
            format_with_separators(i64::try_from(removed).unwrap_or(i64::MAX)),
        )
    }

    fn row(&self, row: usize, line: &DiffLine) -> DiffRowData {
        let file = line
            .file
            .and_then(|file| self.diff.files.get(file).map(|entry| (file, entry)));
        let is_header = line.kind == LineKind::FileHeader;
        let spans = if line.emphasis.is_empty() {
            ModelRc::default()
        } else {
            let spans: Vec<DiffSpan> = split_spans(&line.text, &line.emphasis)
                .into_iter()
                .map(|(text, emphasized)| DiffSpan {
                    text: text.into(),
                    emphasized,
                })
                .collect();
            ModelRc::new(VecModel::from(spans))
        };
        let number = |number: Option<u32>| -> SharedString {
            number
                .map(|number| number.to_string().into())
                .unwrap_or_default()
        };
        DiffRowData {
            kind: DiffRowKind::from(line.kind) as i32,
            file: line
                .file
                .and_then(|file| i32::try_from(file).ok())
                .unwrap_or(-1),
            old_no: number(line.old_no),
            new_no: number(line.new_no),
            text: expand_tabs(&line.text).into(),
            spans,
            status: match file {
                Some((_, entry)) if entry.binary && entry.status == diff::FileStatus::Modified => {
                    "binary".into()
                }
                Some((_, entry)) => entry.status.label().into(),
                None => SharedString::new(),
            },
            added: file.map_or(0, |(_, entry)| {
                i32::try_from(entry.added).unwrap_or(i32::MAX)
            }),
            removed: file.map_or(0, |(_, entry)| {
                i32::try_from(entry.removed).unwrap_or(i32::MAX)
            }),
            collapsed: is_header
                && file.is_some_and(|(file, _)| {
                    self.collapsed.borrow().get(file).copied().unwrap_or(false)
                }),
            can_open: is_header
                && file.is_some_and(|(file, _)| {
                    self.open_targets.get(file).is_some_and(Option::is_some)
                }),
            first: row == 0,
        }
    }
}

impl Model for DiffRowsModel {
    type Data = DiffRowData;

    fn row_count(&self) -> usize {
        self.visible.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        let index = *self.visible.borrow().get(row)?;
        let line = self.diff.lines.get(index)?;
        Some(self.row(row, line))
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Indices of the lines shown when the files flagged in `collapsed` only
/// show their header.
fn visible_rows(lines: &[DiffLine], collapsed: &[bool]) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            line.kind == LineKind::FileHeader
                || line
                    .file
                    .is_none_or(|file| !collapsed.get(file).copied().unwrap_or(false))
        })
        .map(|(index, _)| index)
        .collect()
}

/// Splits `text` into `(segment, emphasized)` runs at the `emphasis` byte
/// ranges, expanding tabs in each segment.
fn split_spans(text: &str, emphasis: &[Range<usize>]) -> Vec<(String, bool)> {
    let mut spans = Vec::new();
    let mut offset = 0;
    for range in emphasis {
        let start = range.start.clamp(offset, text.len());
        let end = range.end.clamp(start, text.len());
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            continue;
        }
        if start > offset {
            spans.push((expand_tabs(&text[offset..start]), false));
        }
        if end > start {
            spans.push((expand_tabs(&text[start..end]), true));
        }
        offset = end;
    }
    if offset < text.len() {
        spans.push((expand_tabs(&text[offset..]), false));
    }
    spans
}

fn expand_tabs(text: &str) -> String {
    if text.contains('\t') {
        text.replace('\t', &" ".repeat(TAB_WIDTH))
    } else {
        text.to_string()
    }
}

fn expanded_width(text: &str) -> usize {
    text.chars()
        .map(|ch| if ch == '\t' { TAB_WIDTH } else { 1 })
        .sum()
}

/// Absolute path for a diff path, resolving relative ones against `base`.
fn resolve_path(path: &str, base: Option<&Path>) -> Option<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        base.map(|base| base.join(path))
    }
}

impl AppController {
    /// Parses the diff of tab `index`, off the UI thread when it is large.
    pub(super) fn files_parse_diff(&mut self, index: usize) {
        let Some(tab_id) = self.tabs.get(index).map(|tab| tab.id) else {
            return;
        };
        let Some(doc) = diff_doc_at(&mut self.tabs, index) else {
            return;
        };
        doc.generation += 1;
        let generation = doc.generation;
        if doc.raw.len() <= SYNC_PARSE_BYTES {
            let parsed = diff::parse_unified_diff(&doc.raw);
            self.files_diff_parsed(tab_id, generation, parsed);
            return;
        }
        let raw = doc.raw.clone();
        self.backend.runtime().spawn_blocking(move || {
            let parsed = diff::parse_unified_diff(&raw);
            crate::ui_thread::post(move |app| app.files_diff_parsed(tab_id, generation, parsed));
        });
    }

    fn files_diff_parsed(&mut self, tab_id: TabId, generation: u64, parsed: ParsedDiff) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let Some(doc) = diff_doc_at(&mut self.tabs, index) else {
            return;
        };
        if doc.generation != generation {
            return;
        }
        let collapsed = doc
            .model
            .as_ref()
            .map(|model| model.collapsed_paths())
            .unwrap_or_default();
        doc.model = Some(Rc::new(DiffRowsModel::new(
            parsed,
            doc.base.as_deref(),
            &collapsed,
        )));
        if self.files_is_shown(index) {
            self.files_push_diff(index, /*switching*/ false);
        }
    }

    /// Pushes diff tab `index` into the `FilesState` global.
    pub(super) fn files_push_diff(&mut self, index: usize, switching: bool) {
        let Some(key) = self.tabs.get(index).map(|tab| tab_key(tab.id)) else {
            return;
        };
        let state = self.window.global::<FilesState>();
        let Some(doc) = diff_doc_at(&mut self.tabs, index) else {
            return;
        };
        state.set_tab_id(key);
        state.set_is_diff(true);
        state.set_title(doc.title.as_str().into());
        state.set_path(
            doc.base
                .as_ref()
                .map(|base| base.display().to_string())
                .unwrap_or_default()
                .into(),
        );
        match &doc.model {
            Some(model) => {
                state.set_diff_rows(ModelRc::from(model.clone()));
                state.set_diff_summary(model.summary().into());
                state.set_diff_hint(
                    if model.row_count() == 0 {
                        "No changes"
                    } else {
                        ""
                    }
                    .into(),
                );
                state.set_diff_max_chars(i32::try_from(model.max_chars).unwrap_or(i32::MAX));
                state.set_diff_number_digits(
                    i32::try_from(model.diff.max_line_number().to_string().len()).unwrap_or(1),
                );
            }
            None => {
                state.set_diff_rows(ModelRc::default());
                state.set_diff_summary(SharedString::new());
                state.set_diff_hint("Reading diff…".into());
            }
        }
        if switching {
            state.set_scroll_x(doc.scroll.0);
            state.set_scroll_y(doc.scroll.1);
            state.set_restore_x(doc.scroll.0);
            state.set_restore_y(doc.scroll.1);
            state.set_restore_pending(true);
        }
    }

    fn files_active_diff_model(&mut self) -> Option<Rc<DiffRowsModel>> {
        let index = self.files_active_index()?;
        diff_doc_at(&mut self.tabs, index)?.model.clone()
    }

    pub(super) fn files_diff_toggle(&mut self, file: i32) {
        if let (Some(model), Ok(file)) = (self.files_active_diff_model(), usize::try_from(file)) {
            model.toggle(file);
        }
    }

    pub(super) fn files_diff_set_collapsed(&mut self, collapsed: bool) {
        if let Some(model) = self.files_active_diff_model() {
            model.set_all_collapsed(collapsed);
            if !collapsed {
                self.window.global::<FilesState>().set_scroll_y(0.0);
            }
        }
    }

    pub(super) fn files_diff_open_file(&mut self, file: i32) {
        let target = self.files_active_diff_model().and_then(|model| {
            usize::try_from(file)
                .ok()
                .and_then(|file| model.open_targets.get(file).cloned().flatten())
        });
        if let Some((path, line)) = target {
            self.open_file_tab(path, line);
        }
    }

    pub(super) fn files_copy_diff(&mut self) {
        let raw = self
            .files_active_index()
            .and_then(|index| diff_doc_at(&mut self.tabs, index))
            .map(|doc| doc.raw.clone());
        if let Some(raw) = raw {
            self.copy_to_clipboard(&raw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const SAMPLE: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n ctx\n-let x = 1;\n+let x = 2;\ndiff --git a/b.rs b/b.rs\nnew file mode 100644\n--- /dev/null\n+++ b/b.rs\n@@ -0,0 +1 @@\n+\tnew\n";

    fn kinds(model: &DiffRowsModel) -> Vec<i32> {
        (0..model.row_count())
            .filter_map(|row| model.row_data(row))
            .map(|row| row.kind)
            .collect()
    }

    #[test]
    fn rows_and_collapse() {
        let model = DiffRowsModel::new(
            diff::parse_unified_diff(SAMPLE),
            Some(Path::new("/repo")),
            &HashSet::new(),
        );
        assert_eq!(kinds(&model), vec![0, 2, 3, 5, 4, 0, 2, 4]);
        assert_eq!(model.summary(), "2 files changed · +2 −1");
        model.toggle(0);
        assert_eq!(kinds(&model), vec![0, 0, 2, 4]);
        let header = model.row_data(0).unwrap_or_default();
        assert!(header.collapsed);
        assert!(header.first);
        assert!(header.can_open);
        model.toggle(0);
        assert_eq!(kinds(&model), vec![0, 2, 3, 5, 4, 0, 2, 4]);
        model.set_all_collapsed(/*collapsed*/ true);
        assert_eq!(kinds(&model), vec![0, 0]);
        assert_eq!(
            model.collapsed_paths(),
            HashSet::from(["a.rs".to_string(), "b.rs".to_string()])
        );
    }

    #[test]
    fn collapsed_paths_survive_reparse() {
        let model = DiffRowsModel::new(
            diff::parse_unified_diff(SAMPLE),
            /*base*/ None,
            &HashSet::from(["b.rs".to_string()]),
        );
        assert_eq!(kinds(&model), vec![0, 2, 3, 5, 4, 0]);
    }

    #[test]
    fn open_targets_point_at_first_change() {
        let model = DiffRowsModel::new(
            diff::parse_unified_diff(SAMPLE),
            Some(Path::new("/repo")),
            &HashSet::new(),
        );
        assert_eq!(
            model.open_targets,
            vec![
                Some((PathBuf::from("/repo/a.rs"), Some(2))),
                Some((PathBuf::from("/repo/b.rs"), Some(1))),
            ]
        );
        let unresolved =
            DiffRowsModel::new(diff::parse_unified_diff(SAMPLE), None, &HashSet::new());
        assert_eq!(unresolved.open_targets, vec![None, None]);
    }

    #[test]
    fn rows_expand_tabs_and_split_spans() {
        let model = DiffRowsModel::new(diff::parse_unified_diff(SAMPLE), None, &HashSet::new());
        let added = model.row_data(7).unwrap_or_default();
        assert_eq!(added.text.as_str(), "    new");
        assert_eq!(added.new_no.as_str(), "1");
        // The longest row is the "@@ -1,2 +1,2 @@" hunk header.
        assert_eq!(model.max_chars, 15);

        let removed = model.row_data(3).unwrap_or_default();
        let spans: Vec<(String, bool)> = removed
            .spans
            .iter()
            .map(|span| (span.text.to_string(), span.emphasized))
            .collect();
        assert_eq!(
            spans,
            vec![
                ("let x = ".to_string(), false),
                ("1".to_string(), true),
                (";".to_string(), false),
            ]
        );
    }

    #[test]
    fn split_spans_handles_edges() {
        assert_eq!(
            split_spans("abc", &[0..1, 2..3]),
            vec![
                ("a".to_string(), true),
                ("b".to_string(), false),
                ("c".to_string(), true),
            ]
        );
        assert_eq!(
            split_spans("é\tx", &[Range { start: 3, end: 4 }]),
            vec![("é    ".to_string(), false), ("x".to_string(), true)]
        );
        // Ranges off char boundaries are ignored rather than panicking.
        assert_eq!(
            split_spans("é", &[Range { start: 1, end: 2 }]),
            vec![("é".to_string(), false)]
        );
    }
}
