//! Splitting and joining the command-line words typed into settings fields:
//! the notification command and MCP server arguments and environment.
//!
//! POSIX shell quoting treats `\` as an escape character, which corrupts
//! Windows paths (`C:\tools\notify.exe` would become `C:toolsnotify.exe`).
//! On Windows the words follow the `CommandLineToArgvW` rules instead:
//! whitespace separates words, double quotes group, and backslashes are
//! literal except directly before a double quote. Elsewhere the usual POSIX
//! shell rules apply. Joining for display uses the matching quoting so the
//! text splits back into the same words.

/// Splits `text` into words with the platform's rules; `None` when a quote
/// is not closed.
pub(crate) fn split_words(text: &str) -> Option<Vec<String>> {
    if cfg!(windows) {
        split_windows(text)
    } else {
        shlex::split(text)
    }
}

/// Joins words for display so that [`split_words`] returns them unchanged.
pub(crate) fn join_words(words: &[String]) -> String {
    if cfg!(windows) {
        join_windows(words)
    } else {
        join_posix(words)
    }
}

fn join_posix(words: &[String]) -> String {
    shlex::try_join(words.iter().map(String::as_str)).unwrap_or_else(|_| words.join(" "))
}

/// `CommandLineToArgvW` / MSVC runtime splitting: `2n` backslashes before a
/// `"` give `n` backslashes and toggle quoting, `2n + 1` give `n` backslashes
/// and a literal `"`, other backslashes are literal, and `""` inside quotes
/// is a literal `"`.
fn split_windows(text: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                in_word = true;
                let mut backslashes = 1;
                while chars.peek() == Some(&'\\') {
                    chars.next();
                    backslashes += 1;
                }
                if chars.peek() == Some(&'"') {
                    word.extend(std::iter::repeat_n('\\', backslashes / 2));
                    if backslashes % 2 == 1 {
                        chars.next();
                        word.push('"');
                    }
                } else {
                    word.extend(std::iter::repeat_n('\\', backslashes));
                }
            }
            '"' => {
                in_word = true;
                if quoted && chars.peek() == Some(&'"') {
                    chars.next();
                    word.push('"');
                } else {
                    quoted = !quoted;
                }
            }
            ' ' | '\t' | '\n' | '\r' if !quoted => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            other => {
                in_word = true;
                word.push(other);
            }
        }
    }
    if quoted {
        return None;
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

fn join_windows(words: &[String]) -> String {
    words
        .iter()
        .map(|word| quote_windows(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quotes one word for [`split_windows`] (the `ArgvQuote` algorithm).
fn quote_windows(word: &str) -> String {
    if !word.is_empty() && !word.contains([' ', '\t', '\n', '\r', '"']) {
        return word.to_string();
    }
    let mut out = String::with_capacity(word.len() + 2);
    out.push('"');
    let mut backslashes = 0;
    for ch in word.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            other => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                out.push(other);
            }
        }
    }
    // Backslashes before the closing quote must be doubled.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn windows_splitting_keeps_backslashes_in_paths() {
        assert_eq!(
            split_windows(r"C:\tools\notify.exe --flag"),
            Some(strings(&[r"C:\tools\notify.exe", "--flag"]))
        );
        assert_eq!(
            split_windows(r"--root C:\work\repo\"),
            Some(strings(&["--root", r"C:\work\repo\"]))
        );
        assert_eq!(
            split_windows(r"HOME=C:\Users\me MODE=fast"),
            Some(strings(&[r"HOME=C:\Users\me", "MODE=fast"]))
        );
        assert_eq!(
            split_windows(r#""C:\Program Files\App\app.exe" -x"#),
            Some(strings(&[r"C:\Program Files\App\app.exe", "-x"]))
        );
        assert_eq!(
            split_windows(r#"MODE="a b" say "turn done""#),
            Some(strings(&["MODE=a b", "say", "turn done"]))
        );
    }

    #[test]
    fn windows_splitting_follows_argv_escapes() {
        // 2n + 1 backslashes before a quote: n backslashes and a literal quote.
        assert_eq!(split_windows(r#"a\"b"#), Some(strings(&[r#"a"b"#])));
        // 2n backslashes before a quote: n backslashes, and the quote groups.
        assert_eq!(split_windows(r#""a\\" b"#), Some(strings(&[r"a\", "b"])));
        assert_eq!(
            split_windows(r#""say ""hi""""#),
            Some(strings(&[r#"say "hi""#]))
        );
        assert_eq!(split_windows(r#""""#), Some(strings(&[""])));
        assert_eq!(split_windows("  "), Some(Vec::new()));
        assert_eq!(split_windows(r#""open"#), None);
        // A backslash before the closing quote escapes it, as on Windows.
        assert_eq!(split_windows(r#""C:\dir\""#), None);
    }

    #[test]
    fn joined_words_split_back_unchanged() {
        let samples = [
            strings(&[r"C:\tools\notify.exe", "--flag"]),
            strings(&[r"C:\Program Files\App\", "x y"]),
            strings(&["say", r#"he said "hi""#, ""]),
            strings(&[r#"a\"b"#, r"trailing\\", "tab\there"]),
            strings(&["plain"]),
        ];
        for words in samples {
            assert_eq!(split_windows(&join_windows(&words)), Some(words.clone()));
            assert_eq!(shlex::split(&join_posix(&words)), Some(words.clone()));
            assert_eq!(split_words(&join_words(&words)), Some(words));
        }
        assert_eq!(
            join_windows(&strings(&[r"C:\Program Files\App\", "-x"])),
            r#""C:\Program Files\App\\" -x"#
        );
        assert_eq!(join_windows(&strings(&[r"C:\a\b.exe"])), r"C:\a\b.exe");
        assert_eq!(
            join_posix(&strings(&["say", "turn done"])),
            "say 'turn done'"
        );
    }

    #[test]
    fn platform_splitting_handles_double_quotes_everywhere() {
        assert_eq!(
            split_words(r#"say "turn done""#),
            Some(strings(&["say", "turn done"]))
        );
        assert_eq!(split_words(r#"say "open"#), None);
        if cfg!(windows) {
            assert_eq!(
                split_words(r"C:\tools\notify.exe"),
                Some(strings(&[r"C:\tools\notify.exe"]))
            );
        }
    }
}
