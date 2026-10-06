//! Suggestion popup above the composer: `@` file mentions, the `/` command
//! palette, and `$` skill mentions. The text input keeps keyboard focus; the
//! view forwards Up/Down/Enter/Tab/Esc while the popup is open.

use std::ops::Range;
use std::path::PathBuf;

use slint::ComponentHandle;
use slint::Model;
use slint::SharedString;

use super::attachments::is_image_path;
use super::file_search::FileHit;
use super::input::SkillRef;
use super::presets;
use super::slash;
use super::slash::SlashMatch;
use super::text;
use crate::app::AppController;
use crate::ui::ComposerState;
use crate::ui::ComposerSuggestion;

/// Most rows kept in the popup (the file search itself returns 20).
const MAX_ENTRIES: usize = 50;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PopupKind {
    Files,
    Commands,
    Skills,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PopupEntry {
    File(FileHit),
    Command(SlashMatch),
    Skill(SkillRef, Vec<u32>),
}

impl PopupEntry {
    /// Stable identity used to keep the selection while results update.
    fn key(&self) -> String {
        match self {
            Self::File(hit) => hit.path.clone(),
            Self::Command(found) => found.command.spec().name.to_string(),
            Self::Skill(skill, _) => skill.name.clone(),
        }
    }
}

/// The open popup.
#[derive(Clone, Debug)]
pub(crate) struct Popup {
    pub(crate) kind: PopupKind,
    /// Byte range of the token being completed (sigil included).
    pub(crate) range: Range<usize>,
    pub(crate) query: String,
    pub(crate) entries: Vec<PopupEntry>,
    /// File search still walking for this query.
    pub(crate) searching: bool,
}

/// What the text around the cursor asks for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PopupTarget {
    pub(crate) kind: PopupKind,
    pub(crate) range: Range<usize>,
    pub(crate) query: String,
}

/// Decides which popup (if any) the cursor position calls for.
///
/// `/` only counts at the very start of the message, `@` anywhere a token
/// starts, and `$` only when skills are known.
pub(crate) fn popup_target(text: &str, cursor: usize, skills_known: bool) -> Option<PopupTarget> {
    if let Some(query) = text::slash_query(text, cursor) {
        return Some(PopupTarget {
            kind: PopupKind::Commands,
            range: 0..1 + query.len(),
            query,
        });
    }
    if let Some(token) = text::token_at_cursor(text, cursor, '@') {
        return Some(PopupTarget {
            kind: PopupKind::Files,
            range: token.range,
            query: token.query,
        });
    }
    if skills_known
        && let Some(token) = text::token_at_cursor(text, cursor, '$')
        && token
            .query
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':'))
    {
        return Some(PopupTarget {
            kind: PopupKind::Skills,
            range: token.range,
            query: token.query,
        });
    }
    None
}

/// How accepting a command row uses the text after the command token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CommandAccept {
    /// Run with this argument (possibly empty) and clear the composer.
    WithArgs(String),
    /// Run without arguments and leave this text in the composer: the
    /// command takes none, and the text is the user's draft.
    KeepDraft(String),
}

/// Decides what accepting `command` does with `rest`, the text after the
/// completed command token.
pub(crate) fn command_accept(command: slash::SlashCommand, rest: &str) -> CommandAccept {
    let rest = rest.trim();
    if rest.is_empty() || command.takes_args() {
        CommandAccept::WithArgs(rest.to_string())
    } else {
        CommandAccept::KeepDraft(rest.to_string())
    }
}

/// Skills matching `query`, best first, with highlight indices into `$name`.
pub(crate) fn matching_skills(skills: &[SkillRef], query: &str) -> Vec<PopupEntry> {
    let mut scored: Vec<(u8, usize, PopupEntry)> = skills
        .iter()
        .enumerate()
        .filter_map(|(order, skill)| {
            let (rank, indices) = if query.is_empty() {
                (0, Vec::new())
            } else {
                text::fuzzy_score(&skill.name, query)?
            };
            let indices = indices.into_iter().map(|index| index + 1).collect();
            Some((rank, order, PopupEntry::Skill(skill.clone(), indices)))
        })
        .collect();
    scored.sort_by_key(|(rank, order, _)| (*rank, *order));
    scored
        .into_iter()
        .map(|(_, _, entry)| entry)
        .take(MAX_ENTRIES)
        .collect()
}

impl AppController {
    /// The text or cursor of the composer changed (typing, clicks, arrows).
    pub(super) fn composer_input_changed(&mut self, text: &str, cursor: usize) {
        self.composer_shared.cursor = cursor;
        let Some(index) = self.active_thread_index() else {
            self.composer_close_popup();
            return;
        };
        let cwd = self
            .thread_tab(index)
            .map(|thread| thread.cwd.clone())
            .unwrap_or_default();
        // The folder may have become known since the tab was shown.
        self.composer_ensure_skills(cwd.clone(), /*force_reload*/ false);
        let skills_known = self
            .composer_shared
            .skills
            .get(&cwd)
            .is_some_and(|skills| !skills.is_empty());
        let target = popup_target(text, cursor, skills_known);
        if let Some(target) = &target
            && self.composer_shared.dismissed.as_ref() == Some(target)
        {
            self.composer_close_popup();
            return;
        }
        self.composer_shared.dismissed = None;
        let Some(target) = target else {
            self.composer_close_popup();
            return;
        };
        let unchanged = self
            .composer_shared
            .popup
            .as_ref()
            .is_some_and(|popup| popup.kind == target.kind && popup.query == target.query);
        if unchanged {
            if let Some(popup) = self.composer_shared.popup.as_mut() {
                popup.range = target.range;
            }
            return;
        }
        let previous = self.composer_shared.popup.take();
        let popup = match target.kind {
            PopupKind::Commands => {
                let entries: Vec<PopupEntry> = slash::matching(&target.query)
                    .into_iter()
                    .map(PopupEntry::Command)
                    .collect();
                if entries.is_empty() {
                    // Not a command after all (e.g. a path); stay out of the way.
                    self.composer_close_popup();
                    return;
                }
                Popup {
                    kind: PopupKind::Commands,
                    range: target.range,
                    query: target.query,
                    entries,
                    searching: false,
                }
            }
            PopupKind::Files if cwd.as_os_str().is_empty() => Popup {
                // A resumed thread's folder is unknown until it loads.
                kind: PopupKind::Files,
                range: target.range,
                query: target.query,
                entries: Vec::new(),
                searching: true,
            },
            PopupKind::Files => {
                // A remote server's folder exists only there: it searches.
                let remote = self
                    .backend
                    .uses_remote_workspace()
                    .then_some(&self.backend);
                self.composer_shared
                    .file_search
                    .update(&cwd, &target.query, remote);
                // Keep the previous results until new ones arrive (no flicker).
                let entries = match previous {
                    Some(previous)
                        if previous.kind == PopupKind::Files && !target.query.is_empty() =>
                    {
                        previous.entries
                    }
                    _ => Vec::new(),
                };
                Popup {
                    kind: PopupKind::Files,
                    range: target.range,
                    searching: !target.query.is_empty(),
                    query: target.query,
                    entries,
                }
            }
            PopupKind::Skills => {
                let skills = self
                    .composer_shared
                    .skills
                    .get(&cwd)
                    .cloned()
                    .unwrap_or_default();
                let entries = matching_skills(&skills, &target.query);
                if entries.is_empty() {
                    self.composer_close_popup();
                    return;
                }
                Popup {
                    kind: PopupKind::Skills,
                    range: target.range,
                    query: target.query,
                    entries,
                    searching: false,
                }
            }
        };
        if popup.kind != PopupKind::Files {
            self.composer_shared.file_search.stop();
        }
        self.composer_shared.popup = Some(popup);
        self.composer_push_popup(/*keep_selection*/ false);
    }

    /// Results from the file search session. `walk_complete` when the
    /// whole folder has been searched (local searches report it).
    pub(crate) fn composer_on_file_matches(
        &mut self,
        generation: u64,
        query: &str,
        hits: Vec<FileHit>,
        walk_complete: bool,
    ) {
        let file_search = &mut self.composer_shared.file_search;
        if !file_search.accept_results(generation, query) {
            return;
        }
        let done = walk_complete || file_search.is_done(query);
        let Some(popup) = self.composer_shared.popup.as_mut() else {
            return;
        };
        if popup.kind != PopupKind::Files || popup.query != query {
            return;
        }
        popup.entries = hits
            .into_iter()
            .map(PopupEntry::File)
            .take(MAX_ENTRIES)
            .collect();
        popup.searching = !done;
        self.composer_push_popup(/*keep_selection*/ true);
    }

    /// The file search session finished matching (see
    /// [`super::file_search::FileSearch::finish`]): ends "Searching…".
    pub(crate) fn composer_on_file_search_done(&mut self, generation: u64) {
        let Some(query) = self.composer_shared.file_search.finish(generation) else {
            return;
        };
        let Some(popup) = self.composer_shared.popup.as_mut() else {
            return;
        };
        if popup.kind != PopupKind::Files || popup.query != query || !popup.searching {
            return;
        }
        popup.searching = false;
        self.composer_push_popup(/*keep_selection*/ true);
    }

    /// Esc: hide the popup until the token under the cursor changes.
    pub(super) fn composer_dismiss_popup(&mut self) {
        if let Some(popup) = self.composer_shared.popup.as_ref() {
            self.composer_shared.dismissed = Some(PopupTarget {
                kind: popup.kind,
                range: popup.range.clone(),
                query: popup.query.clone(),
            });
        }
        self.composer_close_popup();
    }

    pub(super) fn composer_close_popup(&mut self) {
        self.composer_shared.file_search.stop();
        self.composer_shared.popup = None;
        self.composer_shared.popup_keys.clear();
        self.window.global::<ComposerState>().set_popup_open(false);
        if self.composer_shared.suggestions_model.row_count() > 0 {
            self.composer_shared.suggestions_model.set_vec(Vec::new());
        }
    }

    /// Accepts row `row` of the popup.
    pub(super) fn composer_accept_suggestion(&mut self, row: usize) {
        let Some(index) = self.active_thread_index() else {
            return;
        };
        let Some(popup) = self.composer_shared.popup.clone() else {
            return;
        };
        let Some(entry) = popup.entries.get(row).cloned() else {
            return;
        };
        let state = self.window.global::<ComposerState>();
        let current = state.get_text().to_string();
        // The token may have moved if the text changed since the popup was
        // computed; recompute it from the last known cursor.
        let range = popup_target(
            &current,
            self.composer_shared.cursor,
            /*skills_known*/ true,
        )
        .filter(|target| target.kind == popup.kind)
        .map(|target| target.range)
        .unwrap_or(popup.range);
        if range.end > current.len() || !current.is_char_boundary(range.start) {
            self.composer_close_popup();
            return;
        }
        self.composer_close_popup();
        match entry {
            PopupEntry::Command(found) => {
                match command_accept(found.command, &current[range.end..]) {
                    CommandAccept::WithArgs(args) => {
                        self.composer_set_text(String::new(), 0);
                        self.composer_run_slash(index, found.command, args);
                    }
                    CommandAccept::KeepDraft(draft) => {
                        // The command token goes; the user's text stays.
                        let cursor = draft.len();
                        self.composer_set_text(draft, cursor);
                        self.composer_run_slash(index, found.command, String::new());
                    }
                }
            }
            PopupEntry::Skill(skill, _) => {
                let (text, cursor) =
                    text::insert_completion(&current, range, &format!("${}", skill.name));
                self.composer_set_text(text, cursor);
            }
            PopupEntry::File(hit) => {
                let full: PathBuf = self
                    .thread_tab(index)
                    .map(|thread| thread.cwd.join(&hit.path))
                    .unwrap_or_else(|| PathBuf::from(&hit.path));
                // A remote server's files cannot be attached from here; they
                // are mentioned by path instead.
                let as_image = !self.backend.uses_remote_workspace()
                    && !hit.is_dir
                    && is_image_path(&full)
                    && image::image_dimensions(&full).is_ok();
                let model = self
                    .thread_tab(index)
                    .and_then(super::toolbar::effective_model);
                if as_image
                    && presets::model_supports_images(
                        &self.composer_shared.models,
                        model.as_deref(),
                    )
                {
                    let (text, cursor) = text::remove_range(&current, range);
                    self.composer_set_text(text, cursor);
                    let tab_id = self.tabs[index].id;
                    self.composer_attach_to_tab(tab_id, vec![full]);
                    return;
                }
                let mut path = hit.path;
                if hit.is_dir && !path.ends_with('/') {
                    path.push('/');
                }
                let (text, cursor) =
                    text::insert_completion(&current, range, &text::quote_path(&path));
                self.composer_set_text(text, cursor);
            }
        }
    }

    /// Mirrors the popup into the view. With `keep_selection`, the selected
    /// row follows its entry when the list changes.
    fn composer_push_popup(&mut self, keep_selection: bool) {
        let state = self.window.global::<ComposerState>();
        let Some(popup) = self.composer_shared.popup.as_ref() else {
            state.set_popup_open(false);
            return;
        };
        let selected_key = keep_selection
            .then(|| usize::try_from(state.get_selected_suggestion()).ok())
            .flatten()
            .and_then(|row| self.composer_shared.popup_keys.get(row).cloned());
        let accent = color_hex(self.window.global::<crate::ui::Theme>().get_accent());
        let rows: Vec<ComposerSuggestion> = popup
            .entries
            .iter()
            .map(|entry| suggestion_row(entry, &accent))
            .collect();
        let keys: Vec<String> = popup.entries.iter().map(PopupEntry::key).collect();
        let selected = selected_key
            .and_then(|key| keys.iter().position(|candidate| *candidate == key))
            .unwrap_or(0);
        let (title, hint) = match popup.kind {
            PopupKind::Commands => ("Commands", String::new()),
            PopupKind::Skills => ("Skills", String::new()),
            PopupKind::Files => {
                let folder = self
                    .active_thread_index()
                    .and_then(|index| self.thread_tab(index))
                    .map(|thread| crate::app::folder_label(&thread.cwd))
                    .unwrap_or_default();
                let hint = if popup.query.is_empty() {
                    format!("Type to search files in {folder}")
                } else if popup.searching {
                    "Searching…".to_string()
                } else {
                    "No matching files".to_string()
                };
                ("Files", hint)
            }
        };
        self.composer_shared.popup_keys = keys;
        let model = &self.composer_shared.suggestions_model;
        if model.row_count() == rows.len() {
            for (row, data) in rows.into_iter().enumerate() {
                if model.row_data(row).as_ref() != Some(&data) {
                    model.set_row_data(row, data);
                }
            }
        } else {
            model.set_vec(rows);
        }
        state.set_popup_title(title.into());
        state.set_popup_hint(hint.into());
        state.set_selected_suggestion(i32::try_from(selected).unwrap_or(0));
        state.set_popup_open(true);
    }
}

fn suggestion_row(entry: &PopupEntry, accent: &str) -> ComposerSuggestion {
    let (label, indices, detail, kind): (String, &[u32], String, &str) = match entry {
        PopupEntry::File(hit) => {
            let label = if hit.is_dir && !hit.path.ends_with('/') {
                format!("{}/", hit.path)
            } else {
                hit.path.clone()
            };
            (
                label,
                &hit.indices,
                String::new(),
                if hit.is_dir { "folder" } else { "file" },
            )
        }
        PopupEntry::Command(found) => {
            let spec = found.command.spec();
            let detail = match found.via_alias {
                Some(alias) => format!("{} (/{alias})", spec.description),
                None => spec.description.to_string(),
            };
            (format!("/{}", spec.name), &found.indices, detail, "command")
        }
        PopupEntry::Skill(skill, indices) => (
            format!("${}", skill.name),
            indices,
            skill.description.clone(),
            "skill",
        ),
    };
    let markdown = text::highlighted_markdown(&label, indices, accent);
    let title = slint::StyledText::from_markdown(&markdown)
        .unwrap_or_else(|_| slint::StyledText::from_plain_text(&label));
    ComposerSuggestion {
        title,
        detail: first_line(&detail).into(),
        kind: SharedString::from(kind),
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default().trim()
}

fn color_hex(color: slint::Color) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        color.red(),
        color.green(),
        color.blue()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn skill(name: &str) -> SkillRef {
        SkillRef {
            name: name.to_string(),
            description: format!("{name} skill"),
            path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
        }
    }

    #[test]
    fn picks_popup_kind_from_cursor() {
        assert_eq!(
            popup_target("/mo", 3, false),
            Some(PopupTarget {
                kind: PopupKind::Commands,
                range: 0..3,
                query: "mo".to_string(),
            })
        );
        assert_eq!(
            popup_target("fix @src/li", 11, false),
            Some(PopupTarget {
                kind: PopupKind::Files,
                range: 4..11,
                query: "src/li".to_string(),
            })
        );
        assert_eq!(popup_target("use $li", 7, false), None);
        assert_eq!(
            popup_target("use $li", 7, true).map(|target| target.kind),
            Some(PopupKind::Skills)
        );
        assert_eq!(popup_target("costs $5.00", 11, true), None);
        assert_eq!(
            popup_target("/review the @code", 17, false).map(|t| t.kind),
            Some(PopupKind::Files)
        );
        assert_eq!(popup_target("plain words", 5, true), None);
    }

    #[test]
    fn accepted_commands_keep_text_they_do_not_take() {
        use slash::SlashCommand;
        assert_eq!(
            command_accept(SlashCommand::Model, ""),
            CommandAccept::WithArgs(String::new())
        );
        // "/mo|del is wrong here": /model runs and the sentence stays.
        assert_eq!(
            command_accept(SlashCommand::Model, " is wrong here"),
            CommandAccept::KeepDraft("is wrong here".to_string())
        );
        assert_eq!(
            command_accept(SlashCommand::Plan, " add retry logic "),
            CommandAccept::WithArgs("add retry logic".to_string())
        );
        assert_eq!(
            command_accept(SlashCommand::Rename, " Auth work"),
            CommandAccept::WithArgs("Auth work".to_string())
        );
    }

    #[test]
    fn skills_are_ranked_and_highlighted() {
        let skills = vec![skill("deploy"), skill("lint-rust"), skill("rust-review")];
        let found = matching_skills(&skills, "rust");
        let names: Vec<String> = found
            .iter()
            .map(|entry| match entry {
                PopupEntry::Skill(skill, _) => skill.name.clone(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            names,
            vec!["rust-review".to_string(), "lint-rust".to_string()]
        );
        match &found[0] {
            PopupEntry::Skill(_, indices) => assert_eq!(indices, &vec![1, 2, 3, 4]),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(matching_skills(&skills, "").len(), 3);
        assert!(matching_skills(&skills, "zzz").is_empty());
    }

    #[test]
    fn rows_show_paths_commands_and_skills() {
        let row = suggestion_row(
            &PopupEntry::File(FileHit {
                path: "docs".to_string(),
                is_dir: true,
                indices: vec![0],
            }),
            "#000000",
        );
        assert_eq!(row.kind, "folder");
        assert_eq!(row.detail, "");
        let command = suggestion_row(
            &PopupEntry::Command(slash::matching("exit").remove(0)),
            "#000000",
        );
        assert_eq!(command.kind, "command");
        assert_eq!(command.detail, "Quit Codex (/exit)");
        assert_eq!(
            color_hex(slint::Color::from_rgb_u8(47, 111, 224)),
            "#2f6fe0"
        );
    }
}
