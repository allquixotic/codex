//! Command output: ANSI stripping, bounded live buffers, and tail views.

/// Live output kept per running command. Older bytes are dropped (the
/// completed item's `aggregated_output` replaces the buffer anyway).
const LIVE_OUTPUT_MAX_BYTES: usize = 256 * 1024;
/// What remains after the live buffer overflows.
const LIVE_OUTPUT_KEEP_BYTES: usize = 192 * 1024;
/// Longest line shown; longer lines are cut in the middle.
const MAX_LINE_CHARS: usize = 2000;

/// Bounded buffer for `item/commandExecution/outputDelta` text.
#[derive(Clone, Debug, Default)]
pub(crate) struct LiveOutput {
    text: String,
    /// Complete lines dropped from the front.
    dropped_lines: usize,
}

impl LiveOutput {
    pub(crate) fn push(&mut self, delta: &str) {
        self.text.push_str(delta);
        if self.text.len() <= LIVE_OUTPUT_MAX_BYTES {
            return;
        }
        let mut cut = self.text.len() - LIVE_OUTPUT_KEEP_BYTES;
        while !self.text.is_char_boundary(cut) {
            cut += 1;
        }
        // Prefer cutting at a line boundary so the first kept line is whole.
        if let Some(newline) = self.text[cut..].find('\n') {
            cut += newline + 1;
        }
        self.dropped_lines += self.text[..cut].matches('\n').count();
        self.text.drain(..cut);
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn dropped_lines(&self) -> usize {
        self.dropped_lines
    }

    pub(crate) fn len(&self) -> usize {
        self.text.len()
    }
}

/// Removes ANSI escape sequences and control characters (except newline
/// and tab), and resolves carriage-return overwrites within each line.
pub(crate) fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' => match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    // CSI: parameters and intermediates, then a final byte.
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    chars.next();
                    // OSC / DCS / APC: until BEL or ESC \.
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            '\u{9b}' => {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            '\n' | '\t' | '\r' => out.push(ch),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    if out.contains('\r') {
        resolve_carriage_returns(&out)
    } else {
        out
    }
}

/// `progress 10%\rprogress 20%` shows as `progress 20%`.
fn resolve_carriage_returns(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            line.rsplit('\r')
                .find(|segment| !segment.is_empty())
                .unwrap_or("")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The last `max_lines` lines of `raw` (ANSI stripped, trailing blank lines
/// dropped) and the number of earlier lines not shown.
pub(crate) fn output_tail(raw: &str, max_lines: usize) -> (String, usize) {
    let trimmed = raw.trim_end_matches(['\n', '\r', ' ', '\t']);
    if trimmed.is_empty() || max_lines == 0 {
        return (String::new(), 0);
    }
    // Find the start of the last `max_lines` lines without scanning it all.
    let mut start = trimmed.len();
    let mut taken = 0;
    while taken < max_lines {
        match trimmed[..start].rfind('\n') {
            Some(newline) => {
                start = newline;
                taken += 1;
            }
            None => {
                start = 0;
                taken = max_lines;
            }
        }
    }
    let tail = trimmed[start..].trim_start_matches('\n');
    let omitted = if start == 0 {
        0
    } else {
        trimmed[..start].matches('\n').count() + 1
    };
    let lines: Vec<String> = strip_ansi(tail)
        .split('\n')
        .map(|line| cap_line(line).into_owned())
        .collect();
    (lines.join("\n"), omitted)
}

fn cap_line(line: &str) -> std::borrow::Cow<'_, str> {
    let count = line.chars().count();
    if count <= MAX_LINE_CHARS {
        return std::borrow::Cow::Borrowed(line);
    }
    let head: String = line.chars().take(MAX_LINE_CHARS / 2).collect();
    let tail: String = line.chars().skip(count - MAX_LINE_CHARS / 2).collect();
    std::borrow::Cow::Owned(format!("{head} … {tail}"))
}

/// Compact duration like the TUI: `250ms`, `1.25s`, `2m 05s`.
pub(crate) fn format_duration_ms(millis: i64) -> String {
    let millis = millis.max(0);
    if millis < 1000 {
        format!("{millis}ms")
    } else if millis < 60_000 {
        format!("{:.2}s", millis as f64 / 1000.0)
    } else {
        let minutes = millis / 60_000;
        let seconds = (millis % 60_000) / 1000;
        format!("{minutes}m {seconds:02}s")
    }
}

/// Turn duration for "Worked for …": `12s`, `1m 05s`, `1h 02m 03s`.
pub(crate) fn format_elapsed_compact(millis: i64) -> String {
    let secs = millis.max(0) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!(
            "{}h {:02}m {:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn strips_csi_osc_and_controls() {
        let raw = "\u{1b}[31mred\u{1b}[0m \u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7} bell\u{7}\tok";
        assert_eq!(strip_ansi(raw), "red link bell\tok");
    }

    #[test]
    fn resolves_carriage_returns() {
        assert_eq!(strip_ansi("10%\r20%\r30%\ndone\r\n"), "30%\ndone\n");
    }

    #[test]
    fn tail_reports_omitted_lines() {
        let raw = "a\nb\nc\nd\n\n";
        assert_eq!(output_tail(raw, 2), ("c\nd".to_string(), 2));
        assert_eq!(output_tail(raw, 10), ("a\nb\nc\nd".to_string(), 0));
        assert_eq!(output_tail("", 3), (String::new(), 0));
    }

    #[test]
    fn live_output_is_bounded() {
        let mut output = LiveOutput::default();
        let line = format!("{}\n", "x".repeat(1023));
        for _ in 0..400 {
            output.push(&line);
        }
        assert!(output.len() <= LIVE_OUTPUT_MAX_BYTES);
        assert!(output.text().starts_with('x'));
        assert_eq!(
            output.dropped_lines() + output.text().matches('\n').count(),
            400
        );
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration_ms(250), "250ms");
        assert_eq!(format_duration_ms(1234), "1.23s");
        assert_eq!(format_duration_ms(125_000), "2m 05s");
        assert_eq!(format_elapsed_compact(65_000), "1m 05s");
        assert_eq!(format_elapsed_compact(3_723_000), "1h 02m 03s");
    }

    #[test]
    fn long_lines_are_cut_in_the_middle() {
        let line = "y".repeat(MAX_LINE_CHARS * 2);
        let (tail, _) = output_tail(&line, 1);
        assert!(tail.contains(" … "));
        assert!(tail.chars().count() < MAX_LINE_CHARS + 10);
    }
}
