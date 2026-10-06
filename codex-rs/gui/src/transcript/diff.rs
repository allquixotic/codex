//! File-change diffs: +/- counts and colored diff lines for patch rows.
//!
//! `FileUpdateChange.diff` is the full new content for `Add`, the full old
//! content for `Delete`, and a unified diff for `Update` (which may end with
//! `\n\nMoved to: …` for renames).

use codex_app_server_protocol::FileUpdateChange;
use codex_app_server_protocol::PatchChangeKind;

use super::blocks::Line;
use super::blocks::line_kind;

/// Display lines of one file's change.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct FileDiff {
    pub(crate) lines: Vec<Line>,
    pub(crate) added: usize,
    pub(crate) removed: usize,
}

/// Unified diff text with a trailing rename note removed.
pub(crate) fn update_diff_text(change: &FileUpdateChange) -> &str {
    match &change.kind {
        PatchChangeKind::Update {
            move_path: Some(move_path),
        } => {
            let note = format!("\n\nMoved to: {}", move_path.display());
            change
                .diff
                .strip_suffix(note.as_str())
                .or_else(|| change.diff.strip_suffix(&format!("{note}\n")))
                .unwrap_or(&change.diff)
        }
        _ => &change.diff,
    }
}

/// (+added, -removed) for one change.
pub(crate) fn line_counts(change: &FileUpdateChange) -> (usize, usize) {
    match change.kind {
        PatchChangeKind::Add => (change.diff.lines().count(), 0),
        PatchChangeKind::Delete => (0, change.diff.lines().count()),
        PatchChangeKind::Update { .. } => {
            let diff = parse_unified(update_diff_text(change), usize::MAX);
            (diff.added, diff.removed)
        }
    }
}

/// Diff lines for display, at most `max_lines` (a note line reports the rest).
pub(crate) fn file_diff(change: &FileUpdateChange, max_lines: usize) -> FileDiff {
    match change.kind {
        PatchChangeKind::Add => content_diff(&change.diff, /*added*/ true, max_lines),
        PatchChangeKind::Delete => content_diff(&change.diff, /*added*/ false, max_lines),
        PatchChangeKind::Update { .. } => parse_unified(update_diff_text(change), max_lines),
    }
}

fn content_diff(content: &str, added: bool, max_lines: usize) -> FileDiff {
    let total = content.lines().count();
    let (sign, kind) = if added {
        ('+', line_kind::ADDED)
    } else {
        ('-', line_kind::REMOVED)
    };
    let mut lines: Vec<Line> = content
        .lines()
        .take(max_lines)
        .enumerate()
        .map(|(index, text)| Line {
            text: format!("{sign}{}", text.replace('\t', "    ")),
            kind,
            gutter: (index + 1).to_string(),
            target: String::new(),
        })
        .collect();
    if total > max_lines {
        lines.push(Line::new(
            format!("… {} more lines", total - max_lines),
            line_kind::NOTE,
        ));
    }
    FileDiff {
        lines,
        added: if added { total } else { 0 },
        removed: if added { 0 } else { total },
    }
}

/// Parses a unified diff; header lines before the first hunk are skipped.
pub(crate) fn parse_unified(diff: &str, max_lines: usize) -> FileDiff {
    let mut out = FileDiff::default();
    let mut old_line = 0usize;
    let mut new_line = 0usize;
    let mut in_hunk = false;
    let mut hunks = 0usize;
    let mut hidden = 0usize;
    let mut push = |out: &mut FileDiff, line: Line| {
        if out.lines.len() < max_lines {
            out.lines.push(line);
        } else {
            hidden += 1;
        }
    };
    for raw in diff.lines() {
        if let Some(header) = raw.strip_prefix("@@") {
            let (old_start, new_start) = parse_hunk_header(header);
            old_line = old_start;
            new_line = new_start;
            in_hunk = true;
            if hunks > 0 {
                push(&mut out, Line::new("…", line_kind::GAP));
            }
            hunks += 1;
            continue;
        }
        if !in_hunk {
            continue;
        }
        let text = raw.replace('\t', "    ");
        match raw.chars().next() {
            Some('+') => {
                out.added += 1;
                push(
                    &mut out,
                    Line {
                        text,
                        kind: line_kind::ADDED,
                        gutter: new_line.to_string(),
                        target: String::new(),
                    },
                );
                new_line += 1;
            }
            Some('-') => {
                out.removed += 1;
                push(
                    &mut out,
                    Line {
                        text,
                        kind: line_kind::REMOVED,
                        gutter: old_line.to_string(),
                        target: String::new(),
                    },
                );
                old_line += 1;
            }
            Some('\\') => push(&mut out, Line::new(text, line_kind::NOTE)),
            Some(' ') | None => {
                push(
                    &mut out,
                    Line {
                        text: if text.is_empty() {
                            " ".to_string()
                        } else {
                            text
                        },
                        kind: line_kind::CONTEXT,
                        gutter: new_line.to_string(),
                        target: String::new(),
                    },
                );
                old_line += 1;
                new_line += 1;
            }
            // A new file header inside a multi-file diff ends the hunk.
            Some(_) => in_hunk = false,
        }
    }
    if hidden > 0 {
        out.lines
            .push(Line::new(format!("… {hidden} more lines"), line_kind::NOTE));
    }
    out
}

/// `-12,3 +14,5 @@ fn x` → (12, 14).
fn parse_hunk_header(header: &str) -> (usize, usize) {
    let mut old_start = 1;
    let mut new_start = 1;
    for part in header.split_whitespace() {
        if part == "@@" {
            break;
        }
        let number = |text: &str| -> Option<usize> { text.split(',').next()?.parse().ok() };
        if let Some(old) = part.strip_prefix('-').and_then(number) {
            old_start = old;
        } else if let Some(new) = part.strip_prefix('+').and_then(number) {
            new_start = new;
        }
    }
    (old_start, new_start)
}

/// A unified diff for any change kind (for the full diff tab).
pub(crate) fn unified_diff(change: &FileUpdateChange) -> String {
    let path = &change.path;
    match &change.kind {
        PatchChangeKind::Add => {
            let count = change.diff.lines().count();
            let mut out = format!("--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{count} @@\n");
            for line in change.diff.lines() {
                out.push('+');
                out.push_str(line);
                out.push('\n');
            }
            out
        }
        PatchChangeKind::Delete => {
            let count = change.diff.lines().count();
            let mut out = format!("--- a/{path}\n+++ /dev/null\n@@ -1,{count} +0,0 @@\n");
            for line in change.diff.lines() {
                out.push('-');
                out.push_str(line);
                out.push('\n');
            }
            out
        }
        PatchChangeKind::Update { move_path } => {
            let body = update_diff_text(change);
            if body.starts_with("---") || body.starts_with("diff ") {
                return body.to_string();
            }
            let target = move_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| path.clone());
            format!("--- a/{path}\n+++ b/{target}\n{body}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn change(kind: PatchChangeKind, diff: &str) -> FileUpdateChange {
        FileUpdateChange {
            path: "src/a.rs".to_string(),
            kind,
            diff: diff.to_string(),
        }
    }

    #[test]
    fn counts_add_delete_and_update() {
        assert_eq!(line_counts(&change(PatchChangeKind::Add, "a\nb\n")), (2, 0));
        assert_eq!(line_counts(&change(PatchChangeKind::Delete, "a\n")), (0, 1));
        let diff = "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n-b\n+c\n+d\n";
        assert_eq!(
            line_counts(&change(PatchChangeKind::Update { move_path: None }, diff)),
            (2, 1)
        );
    }

    #[test]
    fn numbers_lines_and_separates_hunks() {
        let diff = "@@ -10,2 +10,2 @@\n ctx\n-old\n+new\n@@ -40 +40 @@\n x\n\\ No newline at end of file\n";
        let parsed = parse_unified(diff, 100);
        let rendered: Vec<(i32, String, String)> = parsed
            .lines
            .iter()
            .map(|line| (line.kind, line.gutter.clone(), line.text.clone()))
            .collect();
        assert_eq!(
            rendered,
            vec![
                (line_kind::CONTEXT, "10".to_string(), " ctx".to_string()),
                (line_kind::REMOVED, "11".to_string(), "-old".to_string()),
                (line_kind::ADDED, "11".to_string(), "+new".to_string()),
                (line_kind::GAP, String::new(), "…".to_string()),
                (line_kind::CONTEXT, "40".to_string(), " x".to_string()),
                (
                    line_kind::NOTE,
                    String::new(),
                    "\\ No newline at end of file".to_string()
                ),
            ]
        );
    }

    #[test]
    fn long_diffs_are_capped_with_a_note() {
        let content: String = (0..10).map(|i| format!("{i}\n")).collect();
        let diff = file_diff(&change(PatchChangeKind::Add, &content), 3);
        assert_eq!(diff.lines.len(), 4);
        assert_eq!(diff.lines[3].text, "… 7 more lines");
        assert_eq!(diff.added, 10);
    }

    #[test]
    fn strips_moved_to_suffix() {
        let change = change(
            PatchChangeKind::Update {
                move_path: Some(PathBuf::from("src/b.rs")),
            },
            "@@ -1 +1 @@\n-a\n+b\n\n\nMoved to: src/b.rs",
        );
        assert_eq!(update_diff_text(&change), "@@ -1 +1 @@\n-a\n+b\n");
        assert_eq!(
            unified_diff(&change),
            "--- a/src/a.rs\n+++ b/src/b.rs\n@@ -1 +1 @@\n-a\n+b\n"
        );
    }

    #[test]
    fn synthesizes_diffs_for_new_files() {
        assert_eq!(
            unified_diff(&change(PatchChangeKind::Add, "x\ny\n")),
            "--- /dev/null\n+++ b/src/a.rs\n@@ -0,0 +1,2 @@\n+x\n+y\n"
        );
    }
}
