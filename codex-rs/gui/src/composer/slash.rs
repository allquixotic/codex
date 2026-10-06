//! Slash command palette: the GUI equivalents of the TUI's slash commands
//! (GUI.md section 6), their descriptions, parsing, and fuzzy matching.

use super::text::fuzzy_score;

/// A command the palette can run.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum SlashCommand {
    Model,
    Permissions,
    Approve,
    Plan,
    Review,
    New,
    Resume,
    Fork,
    Rename,
    Compact,
    Recap,
    Side,
    Init,
    Diff,
    Mention,
    Copy,
    Export,
    Status,
    Settings,
    Mcp,
    Skills,
    Plugins,
    Hooks,
    Experimental,
    Memories,
    Theme,
    DebugConfig,
    Archive,
    Ps,
    Stop,
    Worktree,
    Logout,
    Quit,
}

/// Static description of one command.
pub(crate) struct SlashSpec {
    pub(crate) command: SlashCommand,
    pub(crate) name: &'static str,
    pub(crate) aliases: &'static [&'static str],
    pub(crate) description: &'static str,
}

/// Presentation order: most used first, like the TUI popup.
pub(crate) const COMMANDS: &[SlashSpec] = &[
    SlashSpec {
        command: SlashCommand::Model,
        name: "model",
        aliases: &[],
        description: "Choose the model and reasoning effort",
    },
    SlashSpec {
        command: SlashCommand::Permissions,
        name: "permissions",
        aliases: &["approvals"],
        description: "Choose what Codex may do without asking",
    },
    SlashSpec {
        command: SlashCommand::Approve,
        name: "approve",
        aliases: &[],
        description: "Approve one retry of a recent auto-review denial",
    },
    SlashSpec {
        command: SlashCommand::Plan,
        name: "plan",
        aliases: &[],
        description: "Toggle Plan mode, or send a message in Plan mode",
    },
    SlashSpec {
        command: SlashCommand::Review,
        name: "review",
        aliases: &[],
        description: "Review my current changes and find issues",
    },
    SlashSpec {
        command: SlashCommand::New,
        name: "new",
        aliases: &["clear"],
        description: "Start a new thread in a new tab (optionally named)",
    },
    SlashSpec {
        command: SlashCommand::Resume,
        name: "resume",
        aliases: &[],
        description: "Find a saved thread in the sidebar",
    },
    SlashSpec {
        command: SlashCommand::Fork,
        name: "fork",
        aliases: &[],
        description: "Fork this thread into a new tab (optionally named)",
    },
    SlashSpec {
        command: SlashCommand::Rename,
        name: "rename",
        aliases: &[],
        description: "Rename this thread",
    },
    SlashSpec {
        command: SlashCommand::Compact,
        name: "compact",
        aliases: &[],
        description: "Summarize the conversation to free up context",
    },
    SlashSpec {
        command: SlashCommand::Recap,
        name: "recap",
        aliases: &[],
        description: "Catch up: summarize progress and the next step",
    },
    SlashSpec {
        command: SlashCommand::Side,
        name: "side",
        aliases: &["btw"],
        description: "Ask a side question in a temporary copy of this thread",
    },
    SlashSpec {
        command: SlashCommand::Init,
        name: "init",
        aliases: &[],
        description: "Create an AGENTS.md file with instructions for Codex",
    },
    SlashSpec {
        command: SlashCommand::Diff,
        name: "diff",
        aliases: &[],
        description: "Show the changes made in this thread",
    },
    SlashSpec {
        command: SlashCommand::Mention,
        name: "mention",
        aliases: &[],
        description: "Mention a file",
    },
    SlashSpec {
        command: SlashCommand::Copy,
        name: "copy",
        aliases: &[],
        description: "Copy the last reply",
    },
    SlashSpec {
        command: SlashCommand::Export,
        name: "export",
        aliases: &[],
        description: "Export the thread as Markdown",
    },
    SlashSpec {
        command: SlashCommand::Status,
        name: "status",
        aliases: &["usage"],
        description: "Show model, token usage and thread details",
    },
    SlashSpec {
        command: SlashCommand::Settings,
        name: "settings",
        aliases: &["config"],
        description: "Open settings",
    },
    SlashSpec {
        command: SlashCommand::Mcp,
        name: "mcp",
        aliases: &[],
        description: "Manage MCP servers",
    },
    SlashSpec {
        command: SlashCommand::Skills,
        name: "skills",
        aliases: &[],
        description: "Manage skills",
    },
    SlashSpec {
        command: SlashCommand::Plugins,
        name: "plugins",
        aliases: &["apps"],
        description: "Browse plugins",
    },
    SlashSpec {
        command: SlashCommand::Hooks,
        name: "hooks",
        aliases: &[],
        description: "View lifecycle hooks",
    },
    SlashSpec {
        command: SlashCommand::Experimental,
        name: "experimental",
        aliases: &["features"],
        description: "Toggle experimental features",
    },
    SlashSpec {
        command: SlashCommand::Memories,
        name: "memories",
        aliases: &[],
        description: "Configure memories or reset them",
    },
    SlashSpec {
        command: SlashCommand::Theme,
        name: "theme",
        aliases: &[],
        description: "Change the appearance",
    },
    SlashSpec {
        command: SlashCommand::DebugConfig,
        name: "debug-config",
        aliases: &[],
        description: "Show config layers and requirement sources",
    },
    SlashSpec {
        command: SlashCommand::Archive,
        name: "archive",
        aliases: &[],
        description: "Archive this thread",
    },
    SlashSpec {
        command: SlashCommand::Ps,
        name: "ps",
        aliases: &[],
        description: "Show background terminals",
    },
    SlashSpec {
        command: SlashCommand::Stop,
        name: "stop",
        aliases: &[],
        description: "Stop all background terminals",
    },
    SlashSpec {
        command: SlashCommand::Worktree,
        name: "worktree",
        aliases: &[],
        description: "Continue this thread in a new Git worktree",
    },
    SlashSpec {
        command: SlashCommand::Logout,
        name: "logout",
        aliases: &["login"],
        description: "Manage your account and sign-in",
    },
    SlashSpec {
        command: SlashCommand::Quit,
        name: "quit",
        aliases: &["exit"],
        description: "Quit Codex",
    },
];

impl SlashCommand {
    pub(crate) fn spec(self) -> &'static SlashSpec {
        COMMANDS
            .iter()
            .find(|spec| spec.command == self)
            .unwrap_or(&COMMANDS[0])
    }

    /// Whether text after the command is its argument, as with the TUI's
    /// inline-argument commands:
    ///
    /// - `/review <what to review>` and `/rename <name>` prefill their dialog,
    /// - `/side <question>` asks it in the side chat,
    /// - `/plan <message>` enters Plan mode and sends the message,
    /// - `/new <name>` and `/fork <name>` name the new thread,
    /// - `/resume <text>` searches the sidebar.
    ///
    /// Text after any other command makes the whole message a normal
    /// message, so "/diff looks wrong in auth.rs" is sent, not swallowed.
    pub(crate) fn takes_args(self) -> bool {
        matches!(
            self,
            Self::Review
                | Self::Rename
                | Self::Side
                | Self::Plan
                | Self::New
                | Self::Fork
                | Self::Resume
        )
    }
}

/// Parses a submitted message as a command: `/name args`.
///
/// Returns `None` for text that is not a known command, and for a command
/// that takes no arguments followed by more text; the message is then sent
/// as a normal message (so pasting `/usr/bin/env` still works).
pub(crate) fn parse(text: &str) -> Option<(SlashCommand, String)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let (name, args) = match rest.find(char::is_whitespace) {
        Some(index) => (&rest[..index], rest[index..].trim()),
        None => (rest, ""),
    };
    let name = name.to_ascii_lowercase();
    let spec = COMMANDS
        .iter()
        .find(|spec| spec.name == name || spec.aliases.contains(&name.as_str()))?;
    if !args.is_empty() && !spec.command.takes_args() {
        return None;
    }
    Some((spec.command, args.to_string()))
}

/// One palette row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SlashMatch {
    pub(crate) command: SlashCommand,
    /// Char indices into `/name` (the leading slash is index 0) to highlight.
    pub(crate) indices: Vec<u32>,
    /// Alias that matched, when the name itself did not.
    pub(crate) via_alias: Option<&'static str>,
}

/// Commands matching `query` (without the slash), best first.
///
/// Prefix matches rank above substring matches, which rank above
/// subsequence ("fuzzy") matches; alias matches rank below name matches of
/// the same kind. Ties keep presentation order.
pub(crate) fn matching(query: &str) -> Vec<SlashMatch> {
    let query = query.to_ascii_lowercase();
    let mut scored: Vec<(u8, usize, SlashMatch)> = Vec::new();
    for (order, spec) in COMMANDS.iter().enumerate() {
        if query.is_empty() {
            scored.push((
                0,
                order,
                SlashMatch {
                    command: spec.command,
                    indices: Vec::new(),
                    via_alias: None,
                },
            ));
            continue;
        }
        if let Some((rank, indices)) = fuzzy_score(spec.name, &query) {
            scored.push((
                rank * 2,
                order,
                SlashMatch {
                    command: spec.command,
                    indices: indices.into_iter().map(|index| index + 1).collect(),
                    via_alias: None,
                },
            ));
            continue;
        }
        let alias = spec
            .aliases
            .iter()
            .filter_map(|alias| fuzzy_score(alias, &query).map(|(rank, _)| (rank, *alias)))
            .min_by_key(|(rank, _)| *rank);
        if let Some((rank, alias)) = alias {
            scored.push((
                rank * 2 + 1,
                order,
                SlashMatch {
                    command: spec.command,
                    indices: Vec::new(),
                    via_alias: Some(alias),
                },
            ));
        }
    }
    scored.sort_by_key(|(rank, order, _)| (*rank, *order));
    scored.into_iter().map(|(_, _, found)| found).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn names(query: &str) -> Vec<&'static str> {
        matching(query)
            .into_iter()
            .map(|found| found.command.spec().name)
            .collect()
    }

    #[test]
    fn parses_known_commands_with_arguments() {
        assert_eq!(parse("/model"), Some((SlashCommand::Model, String::new())));
        assert_eq!(
            parse("/review  focus on auth \n"),
            Some((SlashCommand::Review, "focus on auth".to_string()))
        );
        assert_eq!(
            parse("/Rename New name"),
            Some((SlashCommand::Rename, "New name".to_string()))
        );
        assert_eq!(parse("/exit"), Some((SlashCommand::Quit, String::new())));
        assert_eq!(parse("/clear"), Some((SlashCommand::New, String::new())));
        assert_eq!(
            parse("/btw what does this flag do?"),
            Some((SlashCommand::Side, "what does this flag do?".to_string()))
        );
        assert_eq!(parse("/recap"), Some((SlashCommand::Recap, String::new())));
        assert_eq!(parse("/ps"), Some((SlashCommand::Ps, String::new())));
        assert_eq!(parse("/stop"), Some((SlashCommand::Stop, String::new())));
        assert_eq!(
            parse("/worktree"),
            Some((SlashCommand::Worktree, String::new()))
        );
    }

    #[test]
    fn text_after_a_command_without_arguments_is_a_message() {
        assert_eq!(parse("/model is wrong here, why?"), None);
        assert_eq!(parse("/diff looks off, check auth.rs"), None);
        assert_eq!(parse("/status\nplease"), None);
        assert_eq!(parse("/approve that"), None);
        assert_eq!(
            parse("/model  \n"),
            Some((SlashCommand::Model, String::new()))
        );
        assert_eq!(
            parse("/plan add retry logic to the uploader"),
            Some((
                SlashCommand::Plan,
                "add retry logic to the uploader".to_string()
            ))
        );
        assert_eq!(
            parse("/new Auth refactor"),
            Some((SlashCommand::New, "Auth refactor".to_string()))
        );
        assert_eq!(
            parse("/fork try B"),
            Some((SlashCommand::Fork, "try B".to_string()))
        );
        assert_eq!(
            parse("/resume flaky test"),
            Some((SlashCommand::Resume, "flaky test".to_string()))
        );
    }

    #[test]
    fn unknown_or_non_commands_are_not_parsed() {
        assert_eq!(parse("/usr/bin/env is missing"), None);
        assert_eq!(parse("/nope"), None);
        assert_eq!(parse("hello /model"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn empty_query_lists_everything_in_order() {
        let all = names("");
        assert_eq!(all.len(), COMMANDS.len());
        assert_eq!(all[0], "model");
        assert!(all.contains(&"approve"));
    }

    #[test]
    fn prefix_beats_substring_beats_subsequence() {
        assert_eq!(names("re")[..3], ["review", "resume", "rename"]);
        // "ex" prefixes "export" and "experimental", then the alias "exit".
        assert_eq!(names("ex"), vec!["export", "experimental", "quit"]);
        // Substring: "diff" contains "if".
        assert!(names("if").contains(&"diff"));
        // Subsequence: m-c-p.
        assert_eq!(names("mcp")[0], "mcp");
        assert!(names("dbg").contains(&"debug-config"));
        assert!(names("zzz").is_empty());
        assert_eq!(names("rec")[0], "recap");
        assert_eq!(names("wor")[0], "worktree");
        assert_eq!(names("btw"), vec!["side"]);
    }

    #[test]
    fn highlight_indices_include_the_slash_offset() {
        let found = matching("mo");
        assert_eq!(found[0].command, SlashCommand::Model);
        assert_eq!(found[0].indices, vec![1, 2]);
        let alias = matching("apps");
        assert_eq!(alias[0].command, SlashCommand::Plugins);
        assert_eq!(alias[0].via_alias, Some("apps"));
        assert!(alias[0].indices.is_empty());
    }

    #[test]
    fn every_command_has_a_spec() {
        for spec in COMMANDS {
            assert_eq!(spec.command.spec().name, spec.name);
        }
    }
}
