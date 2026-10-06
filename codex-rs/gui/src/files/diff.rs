//! Unified diff parsing into display lines, per-file stats, and intra-line
//! change highlights for paired removed/added lines.
//!
//! Accepts multi-file git diffs (`turn/diff/updated`), plain unified diffs,
//! and hunk-only diffs. [`file_changes_to_unified_diff`] turns `FileChange`
//! items (where `diff` is the whole file for adds and deletes) into the same
//! format so one renderer handles both.

use std::ops::Range;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use codex_app_server_protocol::FileUpdateChange;
use codex_app_server_protocol::PatchChangeKind;
use similar::Algorithm;
use similar::DiffTag;

/// Lines longer than this are not diffed for intra-line highlights.
const MAX_INTRALINE_BYTES: usize = 2_000;
/// Change blocks with more lines than this skip intra-line highlights.
const MAX_INTRALINE_BLOCK: usize = 200;
/// Time budget for one intra-line diff.
const INTRALINE_TIMEOUT: Duration = Duration::from_millis(20);
/// Above this share of changed bytes the lines are unrelated; highlighting
/// everything would only add noise.
const MAX_CHANGED_RATIO: f64 = 0.6;

/// Kind of one display line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LineKind {
    /// Start of a file section; `text` is the display path.
    FileHeader,
    /// Text outside any hunk (preamble, unknown lines).
    Meta,
    /// `@@ -a,b +c,d @@ heading`.
    Hunk,
    Context,
    Added,
    Removed,
    /// `\ No newline at end of file`.
    NoNewline,
    /// The file is binary; no hunks.
    Binary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
}

impl FileStatus {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Modified => "modified",
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
        }
    }
}

/// One file section of a diff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiffFile {
    /// `None` for `/dev/null`.
    pub(crate) old_path: Option<String>,
    pub(crate) new_path: Option<String>,
    pub(crate) status: FileStatus,
    pub(crate) added: usize,
    pub(crate) removed: usize,
    pub(crate) binary: bool,
}

impl DiffFile {
    fn new() -> Self {
        Self {
            old_path: None,
            new_path: None,
            status: FileStatus::Modified,
            added: 0,
            removed: 0,
            binary: false,
        }
    }

    /// Path to show: `old → new` for renames and copies.
    pub(crate) fn display_path(&self) -> String {
        match (&self.old_path, &self.new_path) {
            (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
            (_, Some(new)) => new.clone(),
            (Some(old), None) => old.clone(),
            (None, None) => "(unnamed file)".to_string(),
        }
    }

    /// Path of the file as it exists after the change, if it still exists.
    pub(crate) fn current_path(&self) -> Option<&str> {
        match self.status {
            FileStatus::Deleted => None,
            _ => self.new_path.as_deref().or(self.old_path.as_deref()),
        }
    }
}

/// One display line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiffLine {
    pub(crate) kind: LineKind,
    pub(crate) file: Option<usize>,
    pub(crate) old_no: Option<u32>,
    pub(crate) new_no: Option<u32>,
    /// Content without the `+`/`-`/` ` marker.
    pub(crate) text: String,
    /// Byte ranges of `text` that changed relative to the paired line.
    pub(crate) emphasis: Vec<Range<usize>>,
}

impl DiffLine {
    fn new(kind: LineKind, file: Option<usize>, text: impl Into<String>) -> Self {
        Self {
            kind,
            file,
            old_no: None,
            new_no: None,
            text: text.into(),
            emphasis: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ParsedDiff {
    pub(crate) files: Vec<DiffFile>,
    pub(crate) lines: Vec<DiffLine>,
}

impl ParsedDiff {
    pub(crate) fn totals(&self) -> (usize, usize) {
        self.files.iter().fold((0, 0), |(added, removed), file| {
            (added + file.added, removed + file.removed)
        })
    }

    /// Largest line number, for sizing the number columns.
    pub(crate) fn max_line_number(&self) -> u32 {
        self.lines
            .iter()
            .flat_map(|line| [line.old_no, line.new_no])
            .flatten()
            .max()
            .unwrap_or(0)
    }
}

/// Counts left in the current hunk.
#[derive(Clone, Copy, Debug)]
struct HunkState {
    old_line: u32,
    new_line: u32,
    old_left: u32,
    new_left: u32,
}

impl HunkState {
    fn open(&self) -> bool {
        self.old_left > 0 || self.new_left > 0
    }
}

#[derive(Default)]
struct Parser {
    out: ParsedDiff,
    file: Option<usize>,
    /// The current file already has a hunk (a following `---` starts a new
    /// file in plain unified diffs).
    file_has_hunks: bool,
    hunk: Option<HunkState>,
    /// Inside a `GIT binary patch` body.
    in_binary_patch: bool,
}

/// Parses `text` into display lines. Never fails: unknown lines become
/// [`LineKind::Meta`].
pub(crate) fn parse_unified_diff(text: &str) -> ParsedDiff {
    let mut parser = Parser::default();
    for line in text.lines() {
        parser.line(line);
    }
    parser.finish()
}

impl Parser {
    fn line(&mut self, line: &str) {
        if let Some(mut hunk) = self.hunk
            && hunk.open()
            && let Some(row) = hunk_line(line, &mut hunk, self.file)
        {
            self.hunk = Some(hunk);
            self.push_counted(row);
            return;
        }
        if let Some(note) = line.strip_prefix("\\ ")
            && self.file.is_some()
        {
            self.out
                .lines
                .push(DiffLine::new(LineKind::NoNewline, self.file, note.trim()));
            return;
        }
        self.hunk = None;

        if let Some(rest) = line.strip_prefix("diff --git ") {
            self.in_binary_patch = false;
            let file = self.start_file();
            let (old, new) = split_git_paths(rest);
            let entry = &mut self.out.files[file];
            entry.old_path = old;
            entry.new_path = new;
            return;
        }
        if self.in_binary_patch {
            // Forward and reverse literals run until the next file.
            return;
        }
        if let Some(rest) = line.strip_prefix("--- ") {
            if self.file.is_none() || self.file_has_hunks {
                self.start_file();
            }
            if let Some(file) = self.current_file() {
                file.old_path = parse_header_path(rest);
                if file.old_path.is_none() {
                    file.status = FileStatus::Added;
                }
            }
            return;
        }
        if let Some(rest) = line.strip_prefix("+++ ")
            && self.file.is_some()
            && !self.file_has_hunks
        {
            if let Some(file) = self.current_file() {
                file.new_path = parse_header_path(rest);
                if file.new_path.is_none() {
                    file.status = FileStatus::Deleted;
                }
            }
            return;
        }
        if line.starts_with("@@")
            && let Some((old_start, old_count, new_start, new_count)) = parse_hunk_header(line)
        {
            if self.file.is_none() {
                self.start_file();
            }
            self.file_has_hunks = true;
            self.hunk = Some(HunkState {
                old_line: old_start,
                new_line: new_start,
                old_left: old_count,
                new_left: new_count,
            });
            self.out
                .lines
                .push(DiffLine::new(LineKind::Hunk, self.file, line));
            return;
        }
        if self.file.is_some() && !self.file_has_hunks && self.extended_header(line) {
            return;
        }
        if line.starts_with("Binary files ") && line.ends_with(" differ") {
            self.mark_binary();
            return;
        }
        if line == "GIT binary patch" {
            self.mark_binary();
            self.in_binary_patch = true;
            return;
        }
        if line.is_empty() && self.file.is_some() {
            return;
        }
        self.out
            .lines
            .push(DiffLine::new(LineKind::Meta, self.file, line));
    }

    /// Git extended header lines between `diff --git` and the first hunk.
    fn extended_header(&mut self, line: &str) -> bool {
        let Some(file) = self.current_file() else {
            return false;
        };
        if line.starts_with("new file mode ") {
            file.status = FileStatus::Added;
        } else if line.starts_with("deleted file mode ") {
            file.status = FileStatus::Deleted;
        } else if let Some(path) = line.strip_prefix("rename from ") {
            file.old_path = Some(unquote(path));
            file.status = FileStatus::Renamed;
        } else if let Some(path) = line.strip_prefix("rename to ") {
            file.new_path = Some(unquote(path));
            file.status = FileStatus::Renamed;
        } else if let Some(path) = line.strip_prefix("copy from ") {
            file.old_path = Some(unquote(path));
            file.status = FileStatus::Copied;
        } else if let Some(path) = line.strip_prefix("copy to ") {
            file.new_path = Some(unquote(path));
            file.status = FileStatus::Copied;
        } else if ![
            "index ",
            "old mode ",
            "new mode ",
            "similarity index ",
            "dissimilarity index ",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        {
            return false;
        }
        true
    }

    fn start_file(&mut self) -> usize {
        let index = self.out.files.len();
        self.out.files.push(DiffFile::new());
        self.out.lines.push(DiffLine::new(
            LineKind::FileHeader,
            Some(index),
            String::new(),
        ));
        self.file = Some(index);
        self.file_has_hunks = false;
        self.hunk = None;
        index
    }

    fn current_file(&mut self) -> Option<&mut DiffFile> {
        self.file.and_then(|index| self.out.files.get_mut(index))
    }

    fn mark_binary(&mut self) {
        if self.file.is_none() {
            self.start_file();
        }
        if let Some(file) = self.current_file() {
            file.binary = true;
        }
        self.out.lines.push(DiffLine::new(
            LineKind::Binary,
            self.file,
            "Binary file not shown",
        ));
    }

    fn push_counted(&mut self, row: DiffLine) {
        if let Some(file) = self.current_file() {
            match row.kind {
                LineKind::Added => file.added += 1,
                LineKind::Removed => file.removed += 1,
                _ => {}
            }
        }
        self.out.lines.push(row);
    }

    fn finish(mut self) -> ParsedDiff {
        // Resolve header text and statuses now that all headers are known.
        for line in &mut self.out.lines {
            if line.kind == LineKind::FileHeader
                && let Some(file) = line.file.and_then(|index| self.out.files.get_mut(index))
            {
                if file.status == FileStatus::Modified
                    && file.old_path.is_some()
                    && file.new_path.is_some()
                    && file.old_path != file.new_path
                {
                    file.status = FileStatus::Renamed;
                }
                line.text = file.display_path();
            }
        }
        add_intraline_emphasis(&mut self.out);
        self.out
    }
}

/// Classifies `line` inside an open hunk, updating the counters. Returns
/// `None` when the line does not belong to the hunk (malformed counts).
fn hunk_line(line: &str, hunk: &mut HunkState, file: Option<usize>) -> Option<DiffLine> {
    let (kind, text) = match line.as_bytes().first() {
        Some(b' ') => (LineKind::Context, &line[1..]),
        // Some tools strip the space from empty context lines.
        None => (LineKind::Context, ""),
        Some(b'-') if hunk.old_left > 0 => (LineKind::Removed, &line[1..]),
        Some(b'+') if hunk.new_left > 0 => (LineKind::Added, &line[1..]),
        _ => return None,
    };
    let mut row = DiffLine::new(kind, file, text);
    match kind {
        LineKind::Context => {
            row.old_no = Some(hunk.old_line);
            row.new_no = Some(hunk.new_line);
            hunk.old_line += 1;
            hunk.new_line += 1;
            hunk.old_left = hunk.old_left.saturating_sub(1);
            hunk.new_left = hunk.new_left.saturating_sub(1);
        }
        LineKind::Removed => {
            row.old_no = Some(hunk.old_line);
            hunk.old_line += 1;
            hunk.old_left -= 1;
        }
        _ => {
            row.new_no = Some(hunk.new_line);
            hunk.new_line += 1;
            hunk.new_left -= 1;
        }
    }
    Some(row)
}

/// Parses `@@ -a[,b] +c[,d] @@` into `(a, b, c, d)`; counts default to 1.
pub(crate) fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ ")?;
    let end = rest.find(" @@")?;
    let mut ranges = rest[..end].split_whitespace();
    let old = ranges.next()?.strip_prefix('-')?;
    let new = ranges.next()?.strip_prefix('+')?;
    let parse = |range: &str| -> Option<(u32, u32)> {
        match range.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((range.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = parse(old)?;
    let (new_start, new_count) = parse(new)?;
    Some((old_start, old_count, new_start, new_count))
}

/// Path from a `---`/`+++` header; `None` for `/dev/null`.
fn parse_header_path(rest: &str) -> Option<String> {
    // Drop a trailing timestamp ("path\t2024-01-01 ...").
    let raw = rest.split('\t').next().unwrap_or(rest).trim_end();
    let path = unquote(raw);
    if path == "/dev/null" {
        return None;
    }
    Some(strip_ab_prefix(&path).to_string())
}

fn strip_ab_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

/// Splits the `a/old b/new` part of a `diff --git` line.
fn split_git_paths(rest: &str) -> (Option<String>, Option<String>) {
    if rest.starts_with('"') {
        let (first, remainder) = take_quoted(rest);
        let second = remainder.trim_start();
        let second = if second.starts_with('"') {
            take_quoted(second).0
        } else {
            second.to_string()
        };
        return (
            Some(strip_ab_prefix(&first).to_string()),
            Some(strip_ab_prefix(&second).to_string()),
        );
    }
    // Unquoted paths may contain spaces. When old and new are the same, the
    // split is the " b/" whose two halves match; otherwise take the last one.
    let candidates: Vec<usize> = rest.match_indices(" b/").map(|(index, _)| index).collect();
    let split = candidates
        .iter()
        .copied()
        .find(|&index| strip_ab_prefix(&rest[..index]) == &rest[index + 3..])
        .or_else(|| candidates.last().copied());
    match split {
        Some(index) => (
            Some(strip_ab_prefix(&rest[..index]).to_string()),
            Some(rest[index + 3..].to_string()),
        ),
        None => (Some(strip_ab_prefix(rest).to_string()), None),
    }
}

/// Removes C-style quoting used by git for unusual paths.
fn unquote(text: &str) -> String {
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        take_quoted(text).0
    } else {
        text.to_string()
    }
}

/// Reads a quoted string at the start of `text`; returns it and the rest.
fn take_quoted(text: &str) -> (String, &str) {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut index = 1;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                return (
                    String::from_utf8_lossy(&out).into_owned(),
                    &text[index + 1..],
                );
            }
            b'\\' if index + 1 < bytes.len() => {
                let escaped = bytes[index + 1];
                index += 2;
                match escaped {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'0'..=b'7' => {
                        let mut value = u32::from(escaped - b'0');
                        let mut digits = 1;
                        while digits < 3
                            && index < bytes.len()
                            && (b'0'..=b'7').contains(&bytes[index])
                        {
                            value = value * 8 + u32::from(bytes[index] - b'0');
                            index += 1;
                            digits += 1;
                        }
                        out.push(u8::try_from(value).unwrap_or(b'?'));
                    }
                    other => out.push(other),
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    (String::from_utf8_lossy(&out).into_owned(), "")
}

/// Pairs each run of removed lines with the added lines that follow it and
/// records which byte ranges changed.
fn add_intraline_emphasis(diff: &mut ParsedDiff) {
    let lines = &mut diff.lines;
    let mut index = 0;
    while index < lines.len() {
        if lines[index].kind != LineKind::Removed {
            index += 1;
            continue;
        }
        let removed_start = index;
        while index < lines.len() && lines[index].kind == LineKind::Removed {
            index += 1;
        }
        let added_start = index;
        while index < lines.len() && lines[index].kind == LineKind::Added {
            index += 1;
        }
        let removed = added_start - removed_start;
        let added = index - added_start;
        if added == 0 || removed + added > MAX_INTRALINE_BLOCK {
            continue;
        }
        for pair in 0..removed.min(added) {
            let (old, new) = (removed_start + pair, added_start + pair);
            if let Some((old_ranges, new_ranges)) =
                intraline_ranges(&lines[old].text, &lines[new].text)
            {
                lines[old].emphasis = old_ranges;
                lines[new].emphasis = new_ranges;
            }
        }
    }
}

/// Changed byte ranges in an old line and in a new line.
pub(crate) type ChangedRanges = (Vec<Range<usize>>, Vec<Range<usize>>);

/// Changed byte ranges of `old` and `new`, or `None` when highlighting
/// would not help (identical, unrelated, or very long lines).
pub(crate) fn intraline_ranges(old: &str, new: &str) -> Option<ChangedRanges> {
    if old == new || old.len() > MAX_INTRALINE_BYTES || new.len() > MAX_INTRALINE_BYTES {
        return None;
    }
    let old_tokens = tokenize(old);
    let new_tokens = tokenize(new);
    let old_offsets = token_offsets(&old_tokens);
    let new_offsets = token_offsets(&new_tokens);
    let ops = similar::capture_diff_slices_deadline(
        Algorithm::Myers,
        &old_tokens,
        &new_tokens,
        Some(Instant::now() + INTRALINE_TIMEOUT),
    );
    let mut old_ranges: Vec<Range<usize>> = Vec::new();
    let mut new_ranges: Vec<Range<usize>> = Vec::new();
    for op in ops {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        if !old_range.is_empty() {
            push_merged(
                &mut old_ranges,
                old_offsets[old_range.start]..old_offsets[old_range.end],
            );
        }
        if !new_range.is_empty() {
            push_merged(
                &mut new_ranges,
                new_offsets[new_range.start]..new_offsets[new_range.end],
            );
        }
    }
    let changed =
        |ranges: &[Range<usize>]| ranges.iter().map(ExactSizeIterator::len).sum::<usize>();
    let ratio = |ranges: &[Range<usize>], total: usize| {
        if total == 0 {
            0.0
        } else {
            changed(ranges) as f64 / total as f64
        }
    };
    if ratio(&old_ranges, old.len()) > MAX_CHANGED_RATIO
        && ratio(&new_ranges, new.len()) > MAX_CHANGED_RATIO
    {
        return None;
    }
    if old_ranges.is_empty() && new_ranges.is_empty() {
        return None;
    }
    Some((old_ranges, new_ranges))
}

fn push_merged(ranges: &mut Vec<Range<usize>>, range: Range<usize>) {
    match ranges.last_mut() {
        Some(last) if last.end == range.start => last.end = range.end,
        _ => ranges.push(range),
    }
}

/// Splits a line into word, whitespace, and single punctuation tokens.
fn tokenize(text: &str) -> Vec<&str> {
    #[derive(PartialEq)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class = |ch: char| {
        if ch.is_alphanumeric() || ch == '_' {
            Class::Word
        } else if ch.is_whitespace() {
            Class::Space
        } else {
            Class::Other
        }
    };
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut current: Option<Class> = None;
    for (offset, ch) in text.char_indices() {
        let next = class(ch);
        let boundary = match &current {
            None => false,
            Some(Class::Other) => true,
            Some(previous) => *previous != next,
        };
        if boundary {
            tokens.push(&text[start..offset]);
            start = offset;
        }
        current = Some(next);
    }
    if start < text.len() {
        tokens.push(&text[start..]);
    }
    tokens
}

/// Byte offset of each token start, plus the end of the text.
fn token_offsets(tokens: &[&str]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(tokens.len() + 1);
    let mut offset = 0;
    offsets.push(0);
    for token in tokens {
        offset += token.len();
        offsets.push(offset);
    }
    offsets
}

/// Renders `FileChange` items as one git-style unified diff. Adds and
/// deletes carry whole file contents; updates carry hunks.
pub(crate) fn file_changes_to_unified_diff(
    changes: &[FileUpdateChange],
    base: Option<&Path>,
) -> String {
    let mut sorted: Vec<&FileUpdateChange> = changes.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut out = String::new();
    for change in sorted {
        let path = display_change_path(&change.path, base);
        match &change.kind {
            PatchChangeKind::Add => {
                out.push_str(&format!("diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n"));
                push_whole_file_hunk(&mut out, &change.diff, '+');
            }
            PatchChangeKind::Delete => {
                out.push_str(&format!(
                    "diff --git a/{path} b/{path}\ndeleted file mode 100644\n--- a/{path}\n+++ /dev/null\n"
                ));
                push_whole_file_hunk(&mut out, &change.diff, '-');
            }
            PatchChangeKind::Update { move_path } => {
                let dest = move_path
                    .as_ref()
                    .map(|dest| display_change_path(&dest.to_string_lossy(), base))
                    .unwrap_or_else(|| path.clone());
                let body = strip_moved_to(&change.diff);
                out.push_str(&format!("diff --git a/{path} b/{dest}\n"));
                if dest != path {
                    out.push_str(&format!("rename from {path}\nrename to {dest}\n"));
                }
                if !body.starts_with("--- ") {
                    out.push_str(&format!("--- a/{path}\n+++ b/{dest}\n"));
                }
                out.push_str(body);
                if !body.is_empty() && !body.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    out
}

fn display_change_path(path: &str, base: Option<&Path>) -> String {
    let display = base
        .and_then(|base| Path::new(path).strip_prefix(base).ok())
        .map(|relative| relative.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    display.replace('\\', "/")
}

/// Removes the `"\n\nMoved to: …"` note the TUI also strips.
fn strip_moved_to(diff: &str) -> &str {
    match diff.rfind("\n\nMoved to: ") {
        Some(index) if !diff[index + 2..].trim_end_matches('\n').contains('\n') => &diff[..index],
        _ => diff,
    }
}

fn push_whole_file_hunk(out: &mut String, content: &str, marker: char) {
    let count = content.lines().count();
    if count == 0 {
        return;
    }
    if marker == '+' {
        out.push_str(&format!("@@ -0,0 +1,{count} @@\n"));
    } else {
        out.push_str(&format!("@@ -1,{count} +0,0 @@\n"));
    }
    for line in content.lines() {
        out.push(marker);
        out.push_str(line);
        out.push('\n');
    }
    if !content.ends_with('\n') {
        out.push_str("\\ No newline at end of file\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn kinds(diff: &ParsedDiff) -> Vec<LineKind> {
        diff.lines.iter().map(|line| line.kind).collect()
    }

    fn numbers(diff: &ParsedDiff) -> Vec<(Option<u32>, Option<u32>)> {
        diff.lines
            .iter()
            .filter(|line| {
                matches!(
                    line.kind,
                    LineKind::Context | LineKind::Added | LineKind::Removed
                )
            })
            .map(|line| (line.old_no, line.new_no))
            .collect()
    }

    #[test]
    fn parses_git_diff_with_line_numbers_and_stats() {
        let diff = parse_unified_diff(
            "diff --git a/src/lib.rs b/src/lib.rs\n\
             index 1111111..2222222 100644\n\
             --- a/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -1,3 +1,4 @@ fn main\n \
             one\n\
             -two\n\
             +TWO\n\
             +three\n \
             four\n",
        );
        assert_eq!(diff.files.len(), 1);
        let file = &diff.files[0];
        assert_eq!(file.old_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(file.new_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(file.status, FileStatus::Modified);
        assert_eq!((file.added, file.removed), (2, 1));
        assert_eq!(
            kinds(&diff),
            vec![
                LineKind::FileHeader,
                LineKind::Hunk,
                LineKind::Context,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Added,
                LineKind::Context,
            ]
        );
        assert_eq!(diff.lines[0].text, "src/lib.rs");
        assert_eq!(diff.lines[1].text, "@@ -1,3 +1,4 @@ fn main");
        assert_eq!(
            numbers(&diff),
            vec![
                (Some(1), Some(1)),
                (Some(2), None),
                (None, Some(2)),
                (None, Some(3)),
                (Some(3), Some(4)),
            ]
        );
        assert_eq!(diff.max_line_number(), 4);
    }

    #[test]
    fn parses_new_deleted_and_renamed_files() {
        let diff = parse_unified_diff(
            "diff --git a/new.txt b/new.txt\n\
             new file mode 100644\n\
             index 0000000..e69de29\n\
             --- /dev/null\n\
             +++ b/new.txt\n\
             @@ -0,0 +1,2 @@\n\
             +hello\n\
             +world\n\
             diff --git a/old.txt b/old.txt\n\
             deleted file mode 100644\n\
             --- a/old.txt\n\
             +++ /dev/null\n\
             @@ -1 +0,0 @@\n\
             -bye\n\
             diff --git a/a b.txt b/c d.txt\n\
             similarity index 90%\n\
             rename from a b.txt\n\
             rename to c d.txt\n\
             --- a/a b.txt\n\
             +++ b/c d.txt\n\
             @@ -1 +1 @@\n\
             -x\n\
             +y\n",
        );
        let statuses: Vec<FileStatus> = diff.files.iter().map(|file| file.status).collect();
        assert_eq!(
            statuses,
            vec![FileStatus::Added, FileStatus::Deleted, FileStatus::Renamed]
        );
        assert_eq!(diff.files[0].old_path, None);
        assert_eq!(diff.files[0].new_path.as_deref(), Some("new.txt"));
        assert_eq!(diff.files[1].new_path, None);
        assert_eq!(diff.files[1].current_path(), None);
        assert_eq!(diff.files[2].display_path(), "a b.txt → c d.txt");
        assert_eq!(diff.totals(), (3, 2));
        let headers: Vec<&str> = diff
            .lines
            .iter()
            .filter(|line| line.kind == LineKind::FileHeader)
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(headers, vec!["new.txt", "old.txt", "a b.txt → c d.txt"]);
    }

    #[test]
    fn turn_diff_rename_without_extended_headers() {
        // `turn/diff/updated` renames only differ in the a/ and b/ paths.
        let diff = parse_unified_diff(
            "diff --git a/one.rs b/two.rs\nindex 1..2\n--- a/one.rs\n+++ b/two.rs\n@@ -1 +1 @@\n-a\n+b\n",
        );
        assert_eq!(diff.files[0].status, FileStatus::Renamed);
        assert_eq!(diff.files[0].display_path(), "one.rs → two.rs");
    }

    #[test]
    fn no_newline_markers_do_not_consume_counts() {
        let diff = parse_unified_diff(
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n",
        );
        assert_eq!(
            kinds(&diff),
            vec![
                LineKind::FileHeader,
                LineKind::Hunk,
                LineKind::Removed,
                LineKind::NoNewline,
                LineKind::Added,
                LineKind::NoNewline,
            ]
        );
        assert_eq!(diff.lines[3].text, "No newline at end of file");
        assert_eq!(numbers(&diff), vec![(Some(1), None), (None, Some(1))]);
    }

    #[test]
    fn dashes_inside_hunk_are_content_not_headers() {
        // A removed line "-- x" shows as "--- x" in the diff.
        let diff = parse_unified_diff("--- a/f\n+++ b/f\n@@ -1,2 +1,1 @@\n--- x\n+++ y\n-z\n");
        assert_eq!(diff.files.len(), 1);
        assert_eq!(
            kinds(&diff)[2..].to_vec(),
            vec![LineKind::Removed, LineKind::Added, LineKind::Removed]
        );
        assert_eq!(diff.lines[2].text, "-- x");
        assert_eq!(diff.lines[3].text, "++ y");
    }

    #[test]
    fn plain_unified_diffs_split_into_files() {
        let diff = parse_unified_diff(
            "--- a/one\t2024-01-01\n+++ b/one\n@@ -1 +1 @@\n-1\n+2\n--- a/two\n+++ b/two\n@@ -1 +1 @@\n-3\n+4\n",
        );
        assert_eq!(diff.files.len(), 2);
        assert_eq!(diff.files[0].new_path.as_deref(), Some("one"));
        assert_eq!(diff.files[1].new_path.as_deref(), Some("two"));
    }

    #[test]
    fn hunk_only_and_preamble_and_binary() {
        let diff = parse_unified_diff("commit abc\n\n@@ -2,2 +2,2 @@\n a\n-b\n+c\n");
        assert_eq!(diff.lines[0].kind, LineKind::Meta);
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].display_path(), "(unnamed file)");

        let binary = parse_unified_diff(
            "diff --git a/img.png b/img.png\nindex 1..2 100644\nBinary files a/img.png and b/img.png differ\n",
        );
        assert!(binary.files[0].binary);
        assert_eq!(kinds(&binary), vec![LineKind::FileHeader, LineKind::Binary]);

        let patch = parse_unified_diff(
            "diff --git a/x.bin b/x.bin\nGIT binary patch\nliteral 3\nKcmZQz\n\nliteral 0\nHcmV?d00001\n\n",
        );
        assert_eq!(kinds(&patch), vec![LineKind::FileHeader, LineKind::Binary]);
    }

    #[test]
    fn quoted_paths_are_unescaped() {
        let diff = parse_unified_diff(
            "diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"\n--- \"a/caf\\303\\251.txt\"\n+++ \"b/caf\\303\\251.txt\"\n@@ -1 +1 @@\n-a\n+b\n",
        );
        assert_eq!(diff.files[0].new_path.as_deref(), Some("café.txt"));
        assert_eq!(diff.files[0].status, FileStatus::Modified);
    }

    #[test]
    fn hunk_header_parsing() {
        assert_eq!(
            parse_hunk_header("@@ -1,3 +1,4 @@ fn x"),
            Some((1, 3, 1, 4))
        );
        assert_eq!(parse_hunk_header("@@ -5 +7 @@"), Some((5, 1, 7, 1)));
        assert_eq!(parse_hunk_header("@@ -0,0 +1,2 @@"), Some((0, 0, 1, 2)));
        assert_eq!(parse_hunk_header("@@ bogus @@"), None);
    }

    #[test]
    fn intraline_ranges_cover_changed_tokens() {
        let (old, new) =
            intraline_ranges("let value = foo(1);", "let value = bar(1);").unwrap_or_default();
        assert_eq!(old, vec![12..15]);
        assert_eq!(new, vec![12..15]);

        let (old, new) = intraline_ranges("a.b", "a.b.c").unwrap_or_default();
        assert_eq!(old, Vec::<Range<usize>>::new());
        assert_eq!(new, vec![3..5]);

        // Multibyte text: ranges are byte offsets on char boundaries.
        let (old, new) = intraline_ranges("café = 1", "café = 2").unwrap_or_default();
        assert_eq!(old, vec![8..9]);
        assert_eq!(new, vec![8..9]);

        assert_eq!(intraline_ranges("same", "same"), None);
        assert_eq!(
            intraline_ranges("completely different", "nothing alike here!"),
            None
        );
    }

    #[test]
    fn emphasis_pairs_removed_with_following_added() {
        let diff = parse_unified_diff(
            "@@ -1,2 +1,2 @@\n-let a = 1;\n-let b = 2;\n+let a = 10;\n+let b = 2; // x\n",
        );
        let removed: Vec<&DiffLine> = diff
            .lines
            .iter()
            .filter(|line| line.kind == LineKind::Removed)
            .collect();
        let added: Vec<&DiffLine> = diff
            .lines
            .iter()
            .filter(|line| line.kind == LineKind::Added)
            .collect();
        assert_eq!(removed[0].emphasis, vec![8..9]);
        assert_eq!(added[0].emphasis, vec![8..10]);
        assert_eq!(removed[1].emphasis, Vec::<Range<usize>>::new());
        assert_eq!(added[1].emphasis, vec![10..15]);
    }

    #[test]
    fn file_changes_become_git_diff() {
        let changes = vec![
            FileUpdateChange {
                path: "/repo/src/new.rs".to_string(),
                kind: PatchChangeKind::Add,
                diff: "fn a() {}\nfn b() {}".to_string(),
            },
            FileUpdateChange {
                path: "/repo/gone.txt".to_string(),
                kind: PatchChangeKind::Delete,
                diff: "bye\n".to_string(),
            },
            FileUpdateChange {
                path: "/repo/src/lib.rs".to_string(),
                kind: PatchChangeKind::Update {
                    move_path: Some(PathBuf::from("/repo/src/core.rs")),
                },
                diff: "@@ -1 +1 @@\n-old\n+new\n\n\nMoved to: /repo/src/core.rs".to_string(),
            },
        ];
        let text = file_changes_to_unified_diff(&changes, Some(Path::new("/repo")));
        assert_eq!(
            text,
            "diff --git a/gone.txt b/gone.txt\n\
             deleted file mode 100644\n\
             --- a/gone.txt\n\
             +++ /dev/null\n\
             @@ -1,1 +0,0 @@\n\
             -bye\n\
             diff --git a/src/lib.rs b/src/core.rs\n\
             rename from src/lib.rs\n\
             rename to src/core.rs\n\
             --- a/src/lib.rs\n\
             +++ b/src/core.rs\n\
             @@ -1 +1 @@\n\
             -old\n\
             +new\n\
             diff --git a/src/new.rs b/src/new.rs\n\
             new file mode 100644\n\
             --- /dev/null\n\
             +++ b/src/new.rs\n\
             @@ -0,0 +1,2 @@\n\
             +fn a() {}\n\
             +fn b() {}\n\
             \\ No newline at end of file\n"
        );
        let parsed = parse_unified_diff(&text);
        let statuses: Vec<FileStatus> = parsed.files.iter().map(|file| file.status).collect();
        assert_eq!(
            statuses,
            vec![FileStatus::Deleted, FileStatus::Renamed, FileStatus::Added]
        );
        assert_eq!(parsed.totals(), (3, 2));
    }
}
