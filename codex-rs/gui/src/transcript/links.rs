//! Link detection and classification for transcript text.
//!
//! Web links open in the browser. Everything that looks like a local path
//! (`src/main.rs:42`, `/abs/file.py#L10`, `file:///x`) opens a file tab at
//! the cited line, resolved against the thread's working directory.

use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;

/// What a clicked link refers to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LinkTarget {
    Web(String),
    File {
        path: PathBuf,
        line: Option<usize>,
    },
    /// Not something the GUI can open.
    Unknown(String),
}

const WEB_SCHEMES: [&str; 3] = ["https://", "http://", "mailto:"];

/// Decides how to open `url`, resolving relative paths against `cwd`.
pub(crate) fn classify_link(url: &str, cwd: &Path) -> LinkTarget {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if WEB_SCHEMES.iter().any(|scheme| lower.starts_with(scheme)) {
        return LinkTarget::Web(url.to_string());
    }
    if lower.starts_with("file:") {
        return file_url_target(url).unwrap_or_else(|| LinkTarget::Unknown(url.to_string()));
    }
    if lower.contains("://") || url.is_empty() {
        return LinkTarget::Unknown(url.to_string());
    }
    let raw = percent_decode(url);
    let (path, line) = split_location(&raw);
    if path.is_empty() {
        return LinkTarget::Unknown(url.to_string());
    }
    let path = expand_home(path);
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    LinkTarget::File { path, line }
}

/// A `file:` URL as a path on this machine (`file:///C:/x.rs` and
/// `file://server/share/x.rs` on Windows, `file:///x.rs` and
/// `file://localhost/x.rs` everywhere), with the line from a `#L12`
/// fragment or a `:12` suffix. `None` when the URL names no local file.
fn file_url_target(url: &str) -> Option<LinkTarget> {
    let mut parsed = url::Url::parse(url).ok()?;
    let fragment_line = parsed.fragment().and_then(fragment_line);
    parsed.set_fragment(None);
    parsed.set_query(None);
    let path = parsed.to_file_path().ok()?;
    let with_line = {
        let text = path.to_string_lossy();
        let (trimmed, line) = split_colon_location(&text);
        line.map(|line| (PathBuf::from(trimmed), line))
    };
    Some(match with_line {
        Some((path, line)) => LinkTarget::File {
            path,
            line: fragment_line.or(Some(line)),
        },
        None => LinkTarget::File {
            path,
            line: fragment_line,
        },
    })
}

/// Line of a `L12`, `L12C3` or `L12-L20` fragment.
fn fragment_line(fragment: &str) -> Option<usize> {
    let rest = fragment.strip_prefix('L')?;
    rest.chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// Splits `path:12`, `path:12:3`, `path#L12`, `path#L12C3`, `path#L12-L20`.
pub(crate) fn split_location(raw: &str) -> (&str, Option<usize>) {
    if let Some((path, fragment)) = raw.rsplit_once('#') {
        return (path, fragment_line(fragment));
    }
    split_colon_location(raw)
}

/// Splits `path:12`, `path:12:3` and `path:12-20`.
fn split_colon_location(raw: &str) -> (&str, Option<usize>) {
    // Windows drive letters (`C:\x`) contain a colon that is not a location.
    let mut path = raw;
    let mut numbers: Vec<usize> = Vec::new();
    while let Some((head, tail)) = path.rsplit_once(':') {
        let tail = tail.split('-').next().unwrap_or(tail);
        match tail.parse::<usize>() {
            Ok(number) if !tail.is_empty() && numbers.len() < 2 => {
                numbers.push(number);
                path = head;
            }
            _ => break,
        }
    }
    // `path:12:3` pushes 3 then 12; the line is the last pushed number.
    (path, numbers.last().copied())
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Some(value) = text
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(value);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Destination for an inline code span that cites a file
/// (`src/main.rs:42`, `/abs/path.py`), or `None`.
pub(crate) fn citation_destination(code: &str) -> Option<String> {
    let code = code.trim();
    if code.is_empty()
        || code.len() > 300
        || code.contains(char::is_whitespace)
        || code.contains("://")
        || code.contains([
            '(', ')', '<', '>', '"', '\'', ',', ';', '=', '{', '}', '|', '*', '$', '`',
        ])
    {
        return None;
    }
    let (path, line) = split_location(code);
    let first = path.chars().next()?;
    if !(first.is_alphanumeric() || matches!(first, '.' | '/' | '~' | '_' | '\\')) {
        return None;
    }
    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let has_extension = file_name.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty()
            && (1..=8).contains(&ext.len())
            && ext.chars().all(|ch| ch.is_ascii_alphanumeric())
            && ext.chars().any(|ch| ch.is_ascii_alphabetic())
    });
    let has_separator = path.contains('/') || path.contains('\\');
    if path.contains("::") || path.ends_with('/') {
        return None;
    }
    (has_extension && (has_separator || line.is_some())).then(|| code.to_string())
}

/// Byte ranges of bare web URLs and file citations in plain text, with the
/// link destination for each.
pub(crate) fn autolink_ranges(text: &str) -> Vec<(Range<usize>, String)> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    for token in text.split_inclusive(char::is_whitespace) {
        let start = offset;
        offset += token.len();
        let token = token.trim_end();
        let leading = token.len() - token.trim_start_matches(['(', '[', '"', '\'']).len();
        let token_start = start + leading;
        let core = trim_trailing_punctuation(&token[leading..]);
        if core.is_empty() {
            continue;
        }
        let lower = core.to_ascii_lowercase();
        if lower.starts_with("https://") || lower.starts_with("http://") {
            if core.len() > "https://".len() {
                ranges.push((token_start..token_start + core.len(), core.to_string()));
            }
        } else if core.contains('/')
            && let Some(dest) = citation_destination(core)
        {
            ranges.push((token_start..token_start + core.len(), dest));
        }
    }
    ranges
}

fn trim_trailing_punctuation(token: &str) -> &str {
    let mut end = token.len();
    loop {
        let candidate = &token[..end];
        let Some(last) = candidate.chars().last() else {
            return candidate;
        };
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\'' | ']' | '}' => true,
            ')' => candidate.matches('(').count() < candidate.matches(')').count(),
            _ => false,
        };
        if !strip {
            return candidate;
        }
        end -= last.len_utf8();
    }
}

/// Shortens `path` for display: relative to `cwd` when inside it, `~/…`
/// under the home directory, absolute otherwise.
pub(crate) fn display_path(path: &str, cwd: &Path) -> String {
    let candidate = Path::new(path);
    if candidate.is_relative() {
        return path.to_string();
    }
    if let Ok(relative) = candidate.strip_prefix(cwd)
        && !relative.as_os_str().is_empty()
    {
        return relative.to_string_lossy().into_owned();
    }
    if let Some(home) = dirs::home_dir()
        && let Ok(relative) = candidate.strip_prefix(&home)
    {
        return format!("~/{}", relative.to_string_lossy());
    }
    path.to_string()
}

/// Absolute path for a path printed by the agent.
pub(crate) fn resolve_path(path: &str, cwd: &Path) -> PathBuf {
    let path = expand_home(path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn classifies_web_and_file_links() {
        let cwd = Path::new("/repo");
        assert_eq!(
            classify_link("https://example.com/x", cwd),
            LinkTarget::Web("https://example.com/x".to_string())
        );
        assert_eq!(
            classify_link("src/main.rs:42", cwd),
            LinkTarget::File {
                path: PathBuf::from("/repo/src/main.rs"),
                line: Some(42)
            }
        );
        assert_eq!(
            classify_link("/abs/lib.rs#L7C2", cwd),
            LinkTarget::File {
                path: PathBuf::from("/abs/lib.rs"),
                line: Some(7)
            }
        );
        assert_eq!(
            classify_link("file:///tmp/a%20b.txt", cwd),
            LinkTarget::File {
                path: PathBuf::from("/tmp/a b.txt"),
                line: None
            }
        );
        assert!(matches!(
            classify_link("ftp://x", cwd),
            LinkTarget::Unknown(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn file_urls_resolve_to_local_paths() {
        let cwd = Path::new("/repo");
        assert_eq!(
            classify_link("file:///tmp/a.rs#L10", cwd),
            LinkTarget::File {
                path: PathBuf::from("/tmp/a.rs"),
                line: Some(10)
            }
        );
        assert_eq!(
            classify_link("file://localhost/tmp/a.rs:7", cwd),
            LinkTarget::File {
                path: PathBuf::from("/tmp/a.rs"),
                line: Some(7)
            }
        );
        // A file on another host is not on this machine.
        assert!(matches!(
            classify_link("file://server/share/x.rs", cwd),
            LinkTarget::Unknown(_)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_urls_keep_drive_letters_and_hosts() {
        let cwd = Path::new(r"C:\repo");
        assert_eq!(
            classify_link("file:///C:/proj/a.rs#L3", cwd),
            LinkTarget::File {
                path: PathBuf::from(r"C:\proj\a.rs"),
                line: Some(3)
            }
        );
        assert_eq!(
            classify_link("file://server/share/x.rs", cwd),
            LinkTarget::File {
                path: PathBuf::from(r"\\server\share\x.rs"),
                line: None
            }
        );
    }

    #[test]
    fn splits_line_and_column_suffixes() {
        assert_eq!(split_location("a.rs:12:3"), ("a.rs", Some(12)));
        assert_eq!(split_location("a.rs:12-20"), ("a.rs", Some(12)));
        assert_eq!(split_location("a.rs"), ("a.rs", None));
        assert_eq!(split_location("C:\\x\\a.rs:5"), ("C:\\x\\a.rs", Some(5)));
    }

    #[test]
    fn recognizes_file_citations_in_code() {
        assert_eq!(
            citation_destination("src/main.rs:42"),
            Some("src/main.rs:42".to_string())
        );
        assert_eq!(
            citation_destination("main.rs:42"),
            Some("main.rs:42".to_string())
        );
        assert_eq!(
            citation_destination("/abs/x.py"),
            Some("/abs/x.py".to_string())
        );
        assert_eq!(citation_destination("Cargo.toml"), None);
        assert_eq!(citation_destination("Vec<String>"), None);
        assert_eq!(citation_destination("std::fs::read"), None);
        assert_eq!(citation_destination("v1.2.3"), None);
        assert_eq!(citation_destination("cargo test -p x"), None);
    }

    #[test]
    fn finds_bare_urls_and_paths() {
        let text = "See https://a.b/c). and (src/lib.rs:3), not e.g. this.";
        let ranges: Vec<(&str, String)> = autolink_ranges(text)
            .into_iter()
            .map(|(range, dest)| (&text[range], dest))
            .collect();
        assert_eq!(
            ranges,
            vec![
                ("https://a.b/c", "https://a.b/c".to_string()),
                ("src/lib.rs:3", "src/lib.rs:3".to_string()),
            ]
        );
    }

    #[test]
    fn display_paths_are_relative_to_cwd() {
        let cwd = Path::new("/repo");
        assert_eq!(display_path("/repo/src/a.rs", cwd), "src/a.rs");
        assert_eq!(display_path("src/a.rs", cwd), "src/a.rs");
        assert_eq!(display_path("/other/a.rs", cwd), "/other/a.rs");
    }
}
