//! Text helpers for approval cards: command display, permission rules, and
//! file-change diffs. Mirrors the TUI's wording so both front ends describe a
//! request the same way.

use std::path::Path;

use codex_app_server_protocol::AdditionalFileSystemPermissions;
use codex_app_server_protocol::AdditionalNetworkPermissions;
use codex_app_server_protocol::FileSystemAccessMode;
use codex_app_server_protocol::FileSystemPath;
use codex_app_server_protocol::FileSystemSandboxEntry;
use codex_app_server_protocol::FileSystemSpecialPath;
use codex_app_server_protocol::FileUpdateChange;
use codex_app_server_protocol::PatchChangeKind;

/// Longest diff rendered in a card; the rest is summarized in a note line.
pub(crate) const MAX_DIFF_LINES: usize = 400;
/// Longest single diff line kept (characters).
const MAX_DIFF_LINE_CHARS: usize = 400;
/// Length of command snippets in transcript notices.
const SNIPPET_CHARS: usize = 80;

/// Splits a shlex-joined command the way the TUI does, keeping the original
/// string as a single argument when it does not round-trip.
pub(crate) fn split_command(command: &str) -> Vec<String> {
    let Some(parts) = shlex::split(command) else {
        return vec![command.to_string()];
    };
    match shlex::try_join(parts.iter().map(String::as_str)) {
        Ok(round_trip)
            if round_trip == command
                || (!command.contains(":\\")
                    && shlex::split(&round_trip).as_ref() == Some(&parts)) =>
        {
            parts
        }
        _ => vec![command.to_string()],
    }
}

/// Command as the user should read it: the script of `bash -lc <script>`
/// (and the PowerShell equivalent), otherwise the shell-escaped argv.
pub(crate) fn display_argv(argv: &[String]) -> String {
    if let Some(script) = shell_script(argv) {
        return script.to_string();
    }
    shlex::try_join(argv.iter().map(String::as_str)).unwrap_or_else(|_| argv.join(" "))
}

/// [`display_argv`] for an app-server command string.
pub(crate) fn display_command(command: &str) -> String {
    display_argv(&split_command(command))
}

fn shell_script(argv: &[String]) -> Option<&str> {
    let program = argv.first()?;
    let name = Path::new(program)
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())?;
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    match name {
        "bash" | "zsh" | "sh" => match argv {
            [_, flag, script] if flag == "-lc" || flag == "-c" => Some(script.as_str()),
            _ => None,
        },
        "pwsh" | "powershell" => argv
            .iter()
            .position(|arg| arg.eq_ignore_ascii_case("-command") || arg.eq_ignore_ascii_case("-c"))
            .and_then(|flag| argv.get(flag + 1))
            .map(String::as_str),
        _ => None,
    }
}

/// First line of `text` (with " ..." when more follow), truncated for a
/// one-line notice.
pub(crate) fn snippet(text: &str) -> String {
    let mut lines = text.trim().lines();
    let first = lines.next().unwrap_or_default();
    let line = if lines.next().is_some() {
        format!("{first} ...")
    } else {
        first.to_string()
    };
    crate::app::truncate_chars(&line, SNIPPET_CHARS)
}

/// Wraps `text` in an inline-code span that survives backticks inside it.
pub(crate) fn code_span(text: &str) -> String {
    let longest_run = text
        .split(|ch| ch != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    let fence = "`".repeat(longest_run + 1);
    if longest_run > 0 {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
    }
}

/// Escapes CommonMark punctuation so untrusted text renders literally in
/// inline markdown.
pub(crate) fn escape_markdown(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii_punctuation() {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Whether `url` may be handed to the browser: http(s) only. Other schemes
/// (`file:`, `smb:`, custom app handlers) can launch arbitrary programs
/// through the OS handler, so links from servers and agents never open them.
pub(crate) fn is_openable_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://"))
        && !url.contains(|ch: char| ch.is_whitespace() || ch.is_control())
}

/// `path` relative to `cwd` when inside it, else with the home directory
/// shortened to `~`.
pub(crate) fn display_path(path: &str, cwd: Option<&Path>) -> String {
    let candidate = Path::new(path);
    if let Some(cwd) = cwd
        && let Ok(relative) = candidate.strip_prefix(cwd)
        && !relative.as_os_str().is_empty()
    {
        return relative.display().to_string();
    }
    if let Some(home) = dirs::home_dir()
        && let Ok(relative) = candidate.strip_prefix(&home)
    {
        return if relative.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~{}{}", std::path::MAIN_SEPARATOR, relative.display())
        };
    }
    path.to_string()
}

/// One-line summary of additional permissions, e.g. "network; write `/repo`"
/// (same wording as the TUI's "Permission rule").
pub(crate) fn permissions_rule(
    network: Option<&AdditionalNetworkPermissions>,
    file_system: Option<&AdditionalFileSystemPermissions>,
) -> Option<String> {
    let mut parts = Vec::new();
    if network.and_then(|network| network.enabled).unwrap_or(false) {
        parts.push("network".to_string());
    }
    if let Some(file_system) = file_system {
        let entries = file_system_entries(file_system);
        for (access, prefix) in [
            (FileSystemAccessMode::Read, "read"),
            (FileSystemAccessMode::Write, "write"),
            (FileSystemAccessMode::Deny, "deny read"),
        ] {
            let paths = entries
                .iter()
                .filter(|(mode, _)| *mode == access)
                .map(|(_, label)| label.as_str())
                .collect::<Vec<_>>();
            if !paths.is_empty() {
                parts.push(format!("{prefix} {}", paths.join(", ")));
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// Access entries with display labels. Falls back to the legacy `read` and
/// `write` lists when the server did not send `entries`.
fn file_system_entries(
    file_system: &AdditionalFileSystemPermissions,
) -> Vec<(FileSystemAccessMode, String)> {
    if let Some(entries) = file_system.entries.as_ref() {
        return entries
            .iter()
            .map(|entry| (entry.access, entry_label(entry)))
            .collect();
    }
    let mut entries = legacy_entries(file_system.read.as_deref(), FileSystemAccessMode::Read);
    entries.extend(legacy_entries(
        file_system.write.as_deref(),
        FileSystemAccessMode::Write,
    ));
    entries
}

fn legacy_entries<P: std::fmt::Display>(
    paths: Option<&[P]>,
    mode: FileSystemAccessMode,
) -> Vec<(FileSystemAccessMode, String)> {
    paths
        .unwrap_or_default()
        .iter()
        .map(|path| (mode, code_span(&path.to_string())))
        .collect()
}

fn entry_label(entry: &FileSystemSandboxEntry) -> String {
    match &entry.path {
        FileSystemPath::Path { path } => code_span(path.as_str()),
        FileSystemPath::GlobPattern { pattern } => format!("glob {}", code_span(pattern)),
        FileSystemPath::Special { value } => code_span(&special_path_label(value)),
    }
}

fn special_path_label(value: &FileSystemSpecialPath) -> String {
    let with_subpath = |base: &str, subpath: Option<String>| match subpath {
        Some(subpath) => format!("{base}/{subpath}"),
        None => base.to_string(),
    };
    match value {
        FileSystemSpecialPath::Root => ":root".to_string(),
        FileSystemSpecialPath::Minimal => ":minimal".to_string(),
        FileSystemSpecialPath::ProjectRoots { subpath } => with_subpath(
            ":workspace_roots",
            subpath.as_ref().map(ToString::to_string),
        ),
        FileSystemSpecialPath::Tmpdir => ":tmpdir".to_string(),
        FileSystemSpecialPath::SlashTmp => "/tmp".to_string(),
        FileSystemSpecialPath::Unknown { path, subpath } => {
            with_subpath(path, subpath.as_ref().map(ToString::to_string))
        }
    }
}

/// Kind of a rendered diff line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiffLineKind {
    Context,
    Added,
    Removed,
    Hunk,
    File,
    Note,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiffLine {
    pub(crate) text: String,
    pub(crate) kind: DiffLineKind,
}

impl DiffLine {
    fn new(kind: DiffLineKind, text: impl Into<String>) -> Self {
        let text: String = text.into();
        Self {
            text: crate::app::truncate_chars(&text.replace('\t', "    "), MAX_DIFF_LINE_CHARS),
            kind,
        }
    }
}

/// Counts of added and removed lines of one change.
pub(crate) fn change_stats(change: &FileUpdateChange) -> (usize, usize) {
    match &change.kind {
        PatchChangeKind::Add => (change.diff.lines().count(), 0),
        PatchChangeKind::Delete => (0, change.diff.lines().count()),
        PatchChangeKind::Update { .. } => {
            let parsed = parse_update_diff(update_body(change));
            (parsed.added, parsed.removed)
        }
    }
}

/// An update's unified diff as display lines (file headers dropped) with
/// its counts.
#[derive(Debug, Default, PartialEq)]
struct ParsedDiff {
    lines: Vec<DiffLine>,
    added: usize,
    removed: usize,
}

/// Lines a hunk still expects, from its header; `None` when the header did
/// not say.
#[derive(Clone, Copy, Debug)]
struct HunkBudget {
    old: Option<usize>,
    new: Option<usize>,
}

impl HunkBudget {
    /// `@@ -12,3 +14 @@ fn x` → old 3, new 1 (an omitted count is 1).
    fn parse(header: &str) -> Self {
        let count = |sign: char| -> Option<usize> {
            let range = header
                .trim_start_matches('@')
                .split_whitespace()
                .take_while(|part| *part != "@@")
                .find_map(|part| part.strip_prefix(sign))?;
            match range.split_once(',') {
                Some((_, count)) => count.parse().ok(),
                None => range.parse::<usize>().ok().map(|_| 1),
            }
        };
        Self {
            old: count('-'),
            new: count('+'),
        }
    }

    fn is_open(self) -> bool {
        self.old.is_none() || self.new.is_none()
    }

    fn is_exhausted(self) -> bool {
        self.old == Some(0) && self.new == Some(0)
    }

    /// Consumes `line` when it belongs to this hunk.
    fn take(&mut self, line: &str) -> Option<DiffLineKind> {
        if self.is_exhausted() || line.starts_with("@@") {
            return None;
        }
        let spend = |left: &mut Option<usize>| match left {
            Some(0) => false,
            Some(count) => {
                *count -= 1;
                true
            }
            None => true,
        };
        match line.chars().next() {
            Some('+') => spend(&mut self.new).then_some(DiffLineKind::Added),
            Some('-') => spend(&mut self.old).then_some(DiffLineKind::Removed),
            Some(' ') | None => {
                if self.old == Some(0) || self.new == Some(0) {
                    return None;
                }
                spend(&mut self.old);
                spend(&mut self.new);
                Some(DiffLineKind::Context)
            }
            Some('\\') => Some(DiffLineKind::Note),
            Some(_) => None,
        }
    }
}

/// Whether `lines[index]` starts a `---`/`+++` file header that leads into
/// a hunk. Anything else that starts with `-` or `+` is a changed line.
fn is_file_header(lines: &[&str], index: usize) -> bool {
    matches!(
        (lines.get(index), lines.get(index + 1), lines.get(index + 2)),
        (Some(old), Some(new), Some(hunk))
            if old.starts_with("--- ") && new.starts_with("+++ ") && hunk.starts_with("@@")
    )
}

/// Git metadata lines between files; no content line can start like this.
fn is_git_metadata(line: &str) -> bool {
    const PREFIXES: [&str; 13] = [
        "diff ",
        "index ",
        "old mode ",
        "new mode ",
        "deleted file mode ",
        "new file mode ",
        "similarity index ",
        "dissimilarity index ",
        "rename from ",
        "rename to ",
        "copy from ",
        "copy to ",
        "Binary files ",
    ];
    PREFIXES.iter().any(|prefix| line.starts_with(prefix))
}

/// Parses a unified diff. Lines are matched to hunks by the hunk header's
/// counts, so a removed `-- comment` (diff line `--- comment`) or an added
/// `++i;` (diff line `+++i;`) is never mistaken for a file header: only a
/// `---`/`+++` pair right before a hunk header is dropped. A line that does
/// not fit any hunk is shown rather than hidden.
fn parse_update_diff(diff: &str) -> ParsedDiff {
    let lines: Vec<&str> = diff.lines().collect();
    let mut parsed = ParsedDiff::default();
    let mut hunk: Option<HunkBudget> = None;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let kind = match hunk.as_mut() {
            // Without counts a hunk runs until the next file header.
            Some(budget) if budget.is_open() && is_file_header(&lines, index) => None,
            Some(budget) => budget.take(line),
            None => None,
        };
        let kind = match kind {
            Some(kind) => Some(kind),
            None => {
                hunk = None;
                if line.starts_with("@@") {
                    hunk = Some(HunkBudget::parse(line));
                    Some(DiffLineKind::Hunk)
                } else if is_file_header(&lines, index) {
                    index += 2;
                    continue;
                } else {
                    match line.chars().next() {
                        Some('+') => Some(DiffLineKind::Added),
                        Some('-') => Some(DiffLineKind::Removed),
                        Some('\\') => Some(DiffLineKind::Note),
                        _ if is_git_metadata(line) => None,
                        _ => Some(DiffLineKind::Context),
                    }
                }
            }
        };
        index += 1;
        let Some(kind) = kind else {
            continue;
        };
        match kind {
            DiffLineKind::Added => parsed.added += 1,
            DiffLineKind::Removed => parsed.removed += 1,
            _ => {}
        }
        parsed.lines.push(DiffLine::new(kind, line));
    }
    parsed
}

/// The unified diff of an update without the "Moved to" trailer the server
/// appends for renames.
fn update_body(change: &FileUpdateChange) -> &str {
    if let PatchChangeKind::Update {
        move_path: Some(move_path),
    } = &change.kind
        && let Some(body) = change
            .diff
            .strip_suffix(&format!("\n\nMoved to: {}", move_path.display()))
    {
        return body;
    }
    &change.diff
}

/// Header for one file: "Added src/new.rs (+3 -0)".
pub(crate) fn change_header(change: &FileUpdateChange, cwd: Option<&Path>) -> String {
    let path = display_path(&change.path, cwd);
    let (added, removed) = change_stats(change);
    let verb_and_path = match &change.kind {
        PatchChangeKind::Add => format!("Added {path}"),
        PatchChangeKind::Delete => format!("Deleted {path}"),
        PatchChangeKind::Update {
            move_path: Some(move_path),
        } => format!(
            "Moved {path} → {}",
            display_path(&move_path.to_string_lossy(), cwd)
        ),
        PatchChangeKind::Update { move_path: None } => format!("Edited {path}"),
    };
    format!("{verb_and_path} (+{added} -{removed})")
}

/// Renders file changes as colored diff lines, capped at [`MAX_DIFF_LINES`].
pub(crate) fn diff_lines(changes: &[FileUpdateChange], cwd: Option<&Path>) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    let mut omitted = 0usize;
    for change in changes {
        let mut body = Vec::new();
        match &change.kind {
            PatchChangeKind::Add => {
                body.extend(
                    change
                        .diff
                        .lines()
                        .map(|line| DiffLine::new(DiffLineKind::Added, format!("+{line}"))),
                );
            }
            PatchChangeKind::Delete => {
                body.extend(
                    change
                        .diff
                        .lines()
                        .map(|line| DiffLine::new(DiffLineKind::Removed, format!("-{line}"))),
                );
            }
            PatchChangeKind::Update { .. } => {
                body = parse_update_diff(update_body(change)).lines;
            }
        }
        let room = MAX_DIFF_LINES.saturating_sub(lines.len());
        if room == 0 {
            omitted += body.len() + 1;
            continue;
        }
        lines.push(DiffLine::new(
            DiffLineKind::File,
            change_header(change, cwd),
        ));
        let take = body.len().min(room.saturating_sub(1));
        omitted += body.len() - take;
        lines.extend(body.into_iter().take(take));
    }
    if omitted > 0 {
        lines.push(DiffLine::new(
            DiffLineKind::Note,
            format!("… {omitted} more lines not shown"),
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn strings(parts: &[&str]) -> Vec<String> {
        parts.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn display_command_strips_shell_wrappers() {
        assert_eq!(display_command("/bin/zsh -lc 'ls -la'"), "ls -la");
        assert_eq!(
            display_command("bash -c 'echo hi && pwd'"),
            "echo hi && pwd"
        );
        assert_eq!(display_command("git status"), "git status");
        assert_eq!(
            display_argv(&strings(&[
                "pwsh.exe",
                "-NoProfile",
                "-Command",
                "Get-ChildItem"
            ])),
            "Get-ChildItem"
        );
        assert_eq!(display_argv(&strings(&["echo", "a b"])), "echo 'a b'");
    }

    #[test]
    fn split_command_keeps_unparseable_strings_whole() {
        assert_eq!(
            split_command("echo 'unterminated"),
            strings(&["echo 'unterminated"])
        );
        assert_eq!(split_command("ls -la"), strings(&["ls", "-la"]));
    }

    #[test]
    fn snippet_takes_first_line_and_truncates() {
        assert_eq!(snippet("cat <<EOF\nhello\nEOF"), "cat <<EOF ...");
        let long = "x".repeat(200);
        assert_eq!(snippet(&long).chars().count(), 80);
    }

    #[test]
    fn code_span_escapes_backticks() {
        assert_eq!(code_span("ls"), "`ls`");
        assert_eq!(code_span("echo `date`"), "`` echo `date` ``");
    }

    #[test]
    fn escape_markdown_neutralizes_punctuation() {
        assert_eq!(escape_markdown("a_b *c* <d>"), "a\\_b \\*c\\* \\<d\\>");
        assert_eq!(escape_markdown("plain text"), "plain text");
    }

    #[test]
    fn display_path_prefers_cwd_relative() {
        let cwd = PathBuf::from("/work/repo");
        assert_eq!(display_path("/work/repo/src/a.rs", Some(&cwd)), "src/a.rs");
        assert_eq!(
            display_path("/elsewhere/b.rs", Some(&cwd)),
            "/elsewhere/b.rs"
        );
    }

    #[test]
    fn permissions_rule_matches_tui_wording() -> serde_json::Result<()> {
        let file_system: AdditionalFileSystemPermissions =
            serde_json::from_value(serde_json::json!({
                "read": null,
                "write": null,
                "entries": [
                    {"path": {"type": "path", "path": "/repo"}, "access": "write"},
                    {"path": {"type": "glob_pattern", "pattern": "**/*.env"}, "access": "deny"},
                    {"path": {"type": "special", "value": {"kind": "tmpdir"}}, "access": "read"},
                ],
            }))?;
        let network = AdditionalNetworkPermissions {
            enabled: Some(true),
        };
        assert_eq!(
            permissions_rule(Some(&network), Some(&file_system)),
            Some("network; read `:tmpdir`; write `/repo`; deny read glob `**/*.env`".to_string())
        );
        assert_eq!(
            permissions_rule(/*network*/ None, /*file_system*/ None),
            None
        );
        Ok(())
    }

    #[test]
    fn permissions_rule_falls_back_to_legacy_lists() -> serde_json::Result<()> {
        let file_system: AdditionalFileSystemPermissions =
            serde_json::from_value(serde_json::json!({"read": ["/a"], "write": ["/b", "/c"]}))?;
        assert_eq!(
            permissions_rule(/*network*/ None, Some(&file_system)),
            Some("read `/a`; write `/b`, `/c`".to_string())
        );
        Ok(())
    }

    fn change(path: &str, kind: PatchChangeKind, diff: &str) -> FileUpdateChange {
        FileUpdateChange {
            path: path.to_string(),
            kind,
            diff: diff.to_string(),
        }
    }

    #[test]
    fn diff_lines_render_adds_and_updates() {
        let cwd = PathBuf::from("/repo");
        let changes = vec![
            change("/repo/new.txt", PatchChangeKind::Add, "one\ntwo\n"),
            change(
                "/repo/lib.rs",
                PatchChangeKind::Update {
                    move_path: Some(PathBuf::from("/repo/lib2.rs")),
                },
                "@@ -1,2 +1,2 @@\n-old\n+new\n same\n\n\nMoved to: /repo/lib2.rs",
            ),
        ];
        let lines = diff_lines(&changes, Some(&cwd));
        let rendered: Vec<(DiffLineKind, &str)> = lines
            .iter()
            .map(|line| (line.kind, line.text.as_str()))
            .collect();
        assert_eq!(
            rendered,
            vec![
                (DiffLineKind::File, "Added new.txt (+2 -0)"),
                (DiffLineKind::Added, "+one"),
                (DiffLineKind::Added, "+two"),
                (DiffLineKind::File, "Moved lib.rs → lib2.rs (+1 -1)"),
                (DiffLineKind::Hunk, "@@ -1,2 +1,2 @@"),
                (DiffLineKind::Removed, "-old"),
                (DiffLineKind::Added, "+new"),
                (DiffLineKind::Context, " same"),
            ]
        );
    }

    fn kinds(parsed: &ParsedDiff) -> Vec<(DiffLineKind, &str)> {
        parsed
            .lines
            .iter()
            .map(|line| (line.kind, line.text.as_str()))
            .collect()
    }

    #[test]
    fn changed_lines_that_look_like_file_headers_are_shown_and_counted() {
        // Bare hunks as the app-server sends them: removing a SQL comment
        // (`-- guard`), a YAML separator (`---`) and adding C's `++i;`. The
        // last lines of the first hunk even look like a file header.
        let diff = "@@ -1,5 +1,3 @@\n keep\n--- guard: never drop prod\n----\n+++i;\n-x\n--- tail\n+++ head\n@@ -9 +9 @@\n-y\n+z\n";
        let parsed = parse_update_diff(diff);
        assert_eq!(
            kinds(&parsed),
            vec![
                (DiffLineKind::Hunk, "@@ -1,5 +1,3 @@"),
                (DiffLineKind::Context, " keep"),
                (DiffLineKind::Removed, "--- guard: never drop prod"),
                (DiffLineKind::Removed, "----"),
                (DiffLineKind::Added, "+++i;"),
                (DiffLineKind::Removed, "-x"),
                (DiffLineKind::Removed, "--- tail"),
                (DiffLineKind::Added, "+++ head"),
                (DiffLineKind::Hunk, "@@ -9 +9 @@"),
                (DiffLineKind::Removed, "-y"),
                (DiffLineKind::Added, "+z"),
            ]
        );
        assert_eq!((parsed.added, parsed.removed), (3, 5));
        let update = change(
            "/repo/schema.sql",
            PatchChangeKind::Update { move_path: None },
            diff,
        );
        assert_eq!(change_stats(&update), (3, 5));
        assert_eq!(
            change_header(&update, Some(Path::new("/repo"))),
            "Edited schema.sql (+3 -5)"
        );
    }

    #[test]
    fn file_headers_and_git_metadata_are_dropped() {
        let diff = "diff --git a/a.txt b/a.txt\nindex 123..456 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n\\ No newline at end of file\n--- a/b.txt\n+++ b/b.txt\n@@ -2,2 +2,2 @@\n same\n-two\n+zwei\n";
        let parsed = parse_update_diff(diff);
        assert_eq!(
            kinds(&parsed),
            vec![
                (DiffLineKind::Hunk, "@@ -1 +1 @@"),
                (DiffLineKind::Removed, "-old"),
                (DiffLineKind::Added, "+new"),
                (DiffLineKind::Note, "\\ No newline at end of file"),
                (DiffLineKind::Hunk, "@@ -2,2 +2,2 @@"),
                (DiffLineKind::Context, " same"),
                (DiffLineKind::Removed, "-two"),
                (DiffLineKind::Added, "+zwei"),
            ]
        );
        assert_eq!((parsed.added, parsed.removed), (2, 2));
    }

    #[test]
    fn lines_beyond_a_hunk_are_shown_not_hidden() {
        // The header undercounts; the extra removal must still be visible.
        let parsed = parse_update_diff("@@ -1,1 +1,1 @@\n-a\n+b\n--- c\n");
        assert_eq!(
            kinds(&parsed),
            vec![
                (DiffLineKind::Hunk, "@@ -1,1 +1,1 @@"),
                (DiffLineKind::Removed, "-a"),
                (DiffLineKind::Added, "+b"),
                (DiffLineKind::Removed, "--- c"),
            ]
        );
        // Without counts, a hunk runs until the next file header.
        let open = parse_update_diff("@@ @@\n-a\n--- b\n+++ c\n@@ -1 +1 @@\n-d\n+e\n");
        assert_eq!((open.added, open.removed), (1, 2));
    }

    #[test]
    fn only_http_links_are_openable() {
        assert!(is_openable_url("https://example.com/login"));
        assert!(is_openable_url("HTTP://example.com"));
        for url in [
            "file:///etc/passwd",
            "smb://attacker/share",
            "vscode://ext/install",
            "ms-msdt:/id",
            "javascript:alert(1)",
            "https://example.com/a b",
            "https://example.com/\u{7}",
            "",
        ] {
            assert!(!is_openable_url(url), "{url}");
        }
    }

    #[test]
    fn diff_lines_are_capped_with_a_note() {
        let body = (0..MAX_DIFF_LINES + 50)
            .map(|index| format!("line {index}\n"))
            .collect::<String>();
        let lines = diff_lines(
            &[change("/x", PatchChangeKind::Add, &body)],
            /*cwd*/ None,
        );
        assert_eq!(lines.len(), MAX_DIFF_LINES + 1);
        assert_eq!(
            lines.last().map(|line| (line.kind, line.text.clone())),
            Some((DiffLineKind::Note, "… 51 more lines not shown".to_string()))
        );
    }
}
