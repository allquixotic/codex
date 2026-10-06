//! Composer: message input, `@` file mentions, the `/` command palette,
//! `!` shell commands, image attachments and file drops, and the per-thread
//! model / effort / permission / Plan toolbar.
//!
//! The text lives in the Slint `ComposerState.text` property while a tab is
//! shown and in [`ComposerDraft`] while it is in the background; everything
//! else (attachments, Plan mode, context gauge) always lives in the draft.

mod approve;
mod attachments;
mod drop;
mod file_search;
mod input;
mod names;
mod popup;
mod presets;
mod shell;
mod slash;
mod text;
mod toolbar;

pub(crate) use self::toolbar::effective_approval;
pub(crate) use self::toolbar::effective_effort;
pub(crate) use self::toolbar::effective_model;
pub(crate) use self::toolbar::effective_reviewer;
pub(crate) use self::toolbar::effective_sandbox;

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use codex_app_server_protocol::CollaborationModeMask;
use codex_app_server_protocol::Model as ApiModel;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::UserInput;
use codex_protocol::config_types::ModeKind;
use codex_protocol::openai_models::ReasoningEffort;
use slint::ComponentHandle;
use slint::Model;
use slint::ModelRc;
use slint::SharedString;
use slint::VecModel;

use self::approve::Denial;
use self::attachments::Attachment;
use self::file_search::FileHit;
use self::file_search::FileSearch;
use self::input::SkillRef;
use self::names::PendingName;
use self::popup::Popup;
use self::popup::PopupTarget;
use self::presets::PermissionPreset;
use self::slash::SlashCommand;
use self::text::EnterAction;
use crate::app::AppController;
use crate::app::TabId;
use crate::app::ThreadPhase;
use crate::app::ThreadTab;
use crate::prefs::BusyInput;
use crate::ui::AppState;
use crate::ui::ComposerChip;
use crate::ui::ComposerChoice;
use crate::ui::ComposerState;
use crate::ui::ComposerSuggestion;
use crate::ui::SidebarState;

/// A message whose images are being read and encoded for a remote server.
#[derive(Clone, Debug)]
pub(crate) struct PreparingMessage {
    /// Restored into the composer when preparing fails.
    text: String,
    attachments: Vec<Attachment>,
}

/// Per-tab composer state.
#[derive(Clone, Debug, Default)]
pub(crate) struct ComposerDraft {
    /// Unsent text while the tab is in the background.
    pub(crate) text: String,
    /// Images to send with the next message.
    pub(crate) attachments: Vec<Attachment>,
    /// Collaboration mode of the thread (Plan or Default).
    pub(crate) mode: ModeKind,
    /// Effort to restore when leaving Plan mode.
    pub(crate) pre_plan_effort: Option<Option<ReasoningEffort>>,
    /// Percent of the context window left after the last model call.
    pub(crate) context_left: Option<i64>,
    /// Bumped on every optimistic settings change; a failed update only
    /// rolls back when no newer change happened.
    pub(crate) settings_epoch: u64,
    /// Recent auto-review denials, newest first (`/approve`).
    pub(crate) auto_review_denials: VecDeque<Denial>,
    /// A submitted message waiting for its images to be inlined (remote
    /// servers only); later messages wait so the order is kept.
    pub(crate) preparing: Option<PreparingMessage>,
}

/// Composer state shared by all tabs.
#[derive(Default)]
pub(crate) struct ComposerShared {
    /// `model/list` catalog of the running server.
    pub(crate) models: Vec<ApiModel>,
    /// The Plan preset from `collaborationMode/list`; `None` hides Plan mode.
    pub(crate) plan_mask: Option<CollaborationModeMask>,
    /// The server rejected `thread/settings/update`; use turn overrides.
    pub(crate) settings_update_unsupported: bool,
    /// Whether the server's `guardian_approval` feature (auto-review) is on;
    /// `None` until known or when the server does not list it.
    pub(crate) auto_review_feature: Option<bool>,
    /// Names from `/new <name>` and `/fork <name>` waiting for their thread.
    pub(crate) pending_names: HashMap<TabId, PendingName>,
    /// Enabled skills per thread folder, for `$skill` mentions.
    pub(crate) skills: HashMap<PathBuf, Vec<SkillRef>>,
    skills_loading: HashSet<PathBuf>,
    file_search: FileSearch,
    popup: Option<Popup>,
    /// Identity of each popup row, to keep the selection across updates.
    popup_keys: Vec<String>,
    /// Popup the user closed with Esc; stays closed until the token changes.
    dismissed: Option<PopupTarget>,
    /// Last known cursor (byte offset) in the shown text.
    cursor: usize,
    /// Thread tab whose draft is in the view.
    shown_tab: Option<TabId>,
    /// Separate handle for reading copied files (see `paste_image`).
    clipboard: Option<arboard::Clipboard>,
    /// The user accepted running `!` commands outside the sandbox this
    /// session.
    shell_confirmed: bool,
    cursor_serial: i32,
    focus_serial: i32,
    picker_serial: i32,
    suggestions_model: Rc<VecModel<ComposerSuggestion>>,
    attachments_model: Rc<VecModel<ComposerChip>>,
    models_model: Rc<VecModel<ComposerChoice>>,
    efforts_model: Rc<VecModel<ComposerChoice>>,
    permissions_model: Rc<VecModel<ComposerChoice>>,
}

impl AppController {
    pub(crate) fn composer_bind(&mut self) {
        let state = self.window.global::<ComposerState>();
        let shared = &self.composer_shared;
        state.set_suggestions(ModelRc::from(shared.suggestions_model.clone()));
        state.set_attachments(ModelRc::from(shared.attachments_model.clone()));
        state.set_models(ModelRc::from(shared.models_model.clone()));
        state.set_efforts(ModelRc::from(shared.efforts_model.clone()));
        state.set_permissions(ModelRc::from(shared.permissions_model.clone()));

        state.on_send(|| crate::ui_thread::with_app(AppController::composer_submit));
        state.on_interrupt(|| {
            crate::ui_thread::with_app(|app| {
                if let Some(index) = app.active_thread_index() {
                    app.interrupt_tab(index);
                }
            });
        });
        state.on_input_changed(|text, cursor| {
            let text = text.to_string();
            let cursor = usize::try_from(cursor).unwrap_or_default();
            crate::ui_thread::with_app(move |app| app.composer_input_changed(&text, cursor));
        });
        state.on_accept_suggestion(|row| {
            if let Ok(row) = usize::try_from(row) {
                crate::ui_thread::with_app(move |app| app.composer_accept_suggestion(row));
            }
        });
        state.on_dismiss_suggestions(|| {
            crate::ui_thread::with_app(AppController::composer_dismiss_popup);
        });
        state.on_paste_image(|| {
            // Must answer now: `true` consumes the key so no text is pasted.
            let mut handled = false;
            crate::ui_thread::with_app_now(|app| handled = app.composer_paste_image());
            handled
        });
        state.on_attach(|| crate::ui_thread::with_app(AppController::composer_pick_images));
        state.on_remove_attachment(|row| {
            if let Ok(row) = usize::try_from(row) {
                crate::ui_thread::with_app(move |app| app.composer_remove_attachment(row));
            }
        });
        state.on_choose(|picker, id| {
            let (picker, id) = (picker.to_string(), id.to_string());
            crate::ui_thread::with_app(move |app| app.composer_choose(&picker, &id));
        });
        state.on_toggle_plan(|| crate::ui_thread::with_app(AppController::composer_toggle_plan));
        state.on_enter_action(|enter_sends, shift, command, meta, alt| {
            match text::enter_action(enter_sends, shift, command, meta, alt) {
                EnterAction::Send => 1,
                EnterAction::Newline => 0,
            }
        });
        self.composer_install_drop_handler();
        self.composer_refresh();
    }

    pub(crate) fn composer_on_server_ready(&mut self) {
        self.composer_shared.skills.clear();
        self.composer_shared.skills_loading.clear();
        self.composer_shared.settings_update_unsupported = false;
        self.composer_shared.auto_review_feature = None;
        // Searches of the old server are gone with it.
        self.composer_close_popup();
        self.composer_load_catalogs();
        let dir = attachments::attachment_dir(self.codex_home.as_deref());
        self.backend.spawn(async move {
            let _ =
                tokio::task::spawn_blocking(move || attachments::prune_old_attachments(&dir)).await;
        });
        if let Some(cwd) = self
            .active_thread_index()
            .and_then(|index| self.thread_tab(index))
            .map(|thread| thread.cwd.clone())
        {
            self.composer_ensure_skills(cwd, /*force_reload*/ false);
        }
        self.composer_refresh();
    }

    /// Saves the visible text into the tab it belongs to.
    pub(crate) fn composer_stash(&mut self) {
        let Some(tab_id) = self.composer_shared.shown_tab else {
            return;
        };
        let text = self.window.global::<ComposerState>().get_text().to_string();
        if let Some(index) = self.tab_index_by_id(tab_id)
            && let Some(thread) = self.thread_tab_mut(index)
        {
            thread.composer.text = text;
        }
    }

    /// Shows the active tab's draft and settings.
    pub(crate) fn composer_show(&mut self) {
        let active = self
            .active_thread_index()
            .map(|index| (index, self.tabs[index].id));
        if self.composer_shared.shown_tab != active.map(|(_, id)| id) {
            self.composer_stash();
            self.composer_close_popup();
            self.composer_shared.dismissed = None;
            self.composer_shared.shown_tab = active.map(|(_, id)| id);
            let text = active
                .and_then(|(index, _)| self.thread_tab(index))
                .map(|thread| thread.composer.text.clone())
                .unwrap_or_default();
            let cursor = text.len();
            self.composer_set_text(text, cursor);
        }
        // Every time, not only on tab switches: a resumed or new thread
        // learns its folder after its tab is shown. Cheap when loaded.
        if let Some(cwd) = active
            .and_then(|(index, _)| self.thread_tab(index))
            .map(|thread| thread.cwd.clone())
        {
            self.composer_ensure_skills(cwd, /*force_reload*/ false);
        }
        self.composer_apply_pending_names();
        self.composer_refresh();
    }

    /// Inserts `text` at the cursor of the active composer ("quote in reply",
    /// forwarded content) and focuses it.
    pub(crate) fn composer_insert_text(&mut self, text: &str) {
        let current = self.window.global::<ComposerState>().get_text().to_string();
        let (updated, cursor) = text::insert_at(&current, self.composer_shared.cursor, text);
        self.composer_set_text(updated, cursor);
    }

    pub(crate) fn composer_on_notification(&mut self, notification: &ServerNotification) {
        let thread_id = match notification {
            ServerNotification::TurnStarted(n) => Some(n.thread_id.as_str()),
            ServerNotification::TurnCompleted(n) => Some(n.thread_id.as_str()),
            ServerNotification::ThreadStatusChanged(n) => Some(n.thread_id.as_str()),
            ServerNotification::ThreadClosed(n) => Some(n.thread_id.as_str()),
            ServerNotification::ThreadArchived(n) => Some(n.thread_id.as_str()),
            ServerNotification::ThreadUnarchived(n) => Some(n.thread_id.as_str()),
            ServerNotification::ThreadSettingsUpdated(n) => {
                if let Some(index) = self.tab_index_for_thread(&n.thread_id)
                    && let Some(thread) = self.thread_tab_mut(index)
                {
                    thread.composer.mode = n.thread_settings.collaboration_mode.mode;
                }
                Some(n.thread_id.as_str())
            }
            ServerNotification::ThreadTokenUsageUpdated(n) => {
                if let Some(index) = self.tab_index_for_thread(&n.thread_id)
                    && let Some(thread) = self.thread_tab_mut(index)
                {
                    thread.composer.context_left = presets::context_left_percent(&n.token_usage);
                }
                Some(n.thread_id.as_str())
            }
            ServerNotification::SkillsChanged(_) => {
                self.composer_shared.skills.clear();
                self.composer_shared.skills_loading.clear();
                if let Some(cwd) = self
                    .active_thread_index()
                    .and_then(|index| self.thread_tab(index))
                    .map(|thread| thread.cwd.clone())
                {
                    self.composer_ensure_skills(cwd, /*force_reload*/ true);
                }
                None
            }
            ServerNotification::ItemGuardianApprovalReviewCompleted(review) => {
                self.composer_on_guardian_review(review);
                None
            }
            ServerNotification::FuzzyFileSearchSessionUpdated(updated) => {
                if let Some(generation) = self
                    .composer_shared
                    .file_search
                    .remote_generation(&updated.session_id)
                {
                    let hits = updated.files.iter().map(FileHit::from_remote).collect();
                    self.composer_on_file_matches(
                        generation,
                        &updated.query,
                        hits,
                        /*walk_complete*/ false,
                    );
                }
                None
            }
            ServerNotification::FuzzyFileSearchSessionCompleted(completed) => {
                if let Some(generation) = self
                    .composer_shared
                    .file_search
                    .remote_generation(&completed.session_id)
                {
                    self.composer_on_file_search_done(generation);
                }
                None
            }
            _ => None,
        };
        if thread_id.is_some() {
            self.composer_apply_pending_names();
        }
        let is_active = thread_id.is_some_and(|thread_id| {
            self.active_thread_index()
                .and_then(|index| self.thread_tab(index))
                .and_then(|thread| thread.thread_id.as_deref())
                == Some(thread_id)
        });
        if is_active {
            self.composer_refresh();
        }
    }

    /// Sends the composer content (or runs a `/command`).
    fn composer_submit(&mut self) {
        let Some(index) = self.active_thread_index() else {
            return;
        };
        let state = self.window.global::<ComposerState>();
        let message = state.get_text().to_string();
        if let Some((command, args)) = slash::parse(&message) {
            self.composer_close_popup();
            self.composer_set_text(String::new(), 0);
            self.composer_run_slash(index, command, args);
            return;
        }
        if let Some(command) = shell::parse_shell_command(&message) {
            self.composer_close_popup();
            self.composer_run_shell(index, command, message);
            return;
        }
        self.composer_send_message(index, &message);
    }

    /// Sends `message` with the draft's attachments to tab `index` and
    /// clears the composer. Returns false (the draft stays) when nothing was
    /// sent.
    fn composer_send_message(&mut self, index: usize, message: &str) -> bool {
        let Some(thread) = self.thread_tab(index) else {
            return false;
        };
        if matches!(thread.phase, ThreadPhase::Closed | ThreadPhase::Error) {
            return false;
        }
        if thread.composer.preparing.is_some() {
            self.toast("Still preparing the images of your last message");
            return false;
        }
        let images: Vec<PathBuf> = thread
            .composer
            .attachments
            .iter()
            .map(|attachment| attachment.path.clone())
            .collect();
        let model = toolbar::effective_model(thread);
        if !images.is_empty()
            && !presets::model_supports_images(&self.composer_shared.models, model.as_deref())
        {
            self.toast(format!(
                "{} does not accept images. Remove the attachments or pick another model.",
                presets::model_label(&self.composer_shared.models, model.as_deref())
            ));
            return false;
        }
        let skills = self
            .composer_shared
            .skills
            .get(&thread.cwd)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let input = input::build_user_input(message, &images, skills);
        if input.is_empty() {
            return false;
        }
        self.composer_close_popup();
        self.composer_set_text(String::new(), 0);
        let attachments = self
            .thread_tab_mut(index)
            .map(|thread| std::mem::take(&mut thread.composer.attachments))
            .unwrap_or_default();
        if !images.is_empty() && self.backend.uses_remote_workspace() {
            self.composer_send_inline_images(index, input, message.to_string(), attachments);
        } else if !self.send_user_input(index, input) {
            let draft = PreparingMessage {
                text: message.to_string(),
                attachments,
            };
            self.composer_restore_draft(index, draft);
            self.composer_refresh();
            return false;
        }
        self.composer_refresh();
        true
    }

    /// A remote server cannot read this machine's files: reads and encodes
    /// the attached images off the UI thread (as data URLs, like the TUI's
    /// remote mode), then sends the message.
    fn composer_send_inline_images(
        &mut self,
        index: usize,
        input: Vec<UserInput>,
        text: String,
        attachments: Vec<Attachment>,
    ) {
        let tab_id = self.tabs[index].id;
        if let Some(thread) = self.thread_tab_mut(index) {
            thread.composer.preparing = Some(PreparingMessage { text, attachments });
        }
        self.backend.spawn(async move {
            let prepared = tokio::task::spawn_blocking(move || {
                input::inline_local_images(input, input::MAX_INLINE_IMAGE_BYTES)
            })
            .await
            .unwrap_or_else(|err| Err(format!("Could not prepare the images: {err}")));
            crate::ui_thread::post(move |app| app.composer_on_images_inlined(tab_id, prepared));
        });
    }

    fn composer_on_images_inlined(
        &mut self,
        tab_id: TabId,
        prepared: Result<Vec<UserInput>, String>,
    ) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let Some(preparing) = self
            .thread_tab_mut(index)
            .and_then(|thread| thread.composer.preparing.take())
        else {
            return;
        };
        match prepared {
            Ok(input) => {
                if !self.send_user_input(index, input) {
                    self.composer_restore_draft(index, preparing);
                }
            }
            Err(err) => {
                self.toast(err);
                self.composer_restore_draft(index, preparing);
            }
        }
        if self.active == Some(index) {
            self.composer_refresh();
        }
    }

    /// Puts an unsent message back: its attachments always, its text when
    /// the composer is still empty.
    fn composer_restore_draft(&mut self, index: usize, draft: PreparingMessage) {
        let shown = self.active == Some(index);
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let mut attachments = draft.attachments;
        attachments.retain(|restored| {
            !thread
                .composer
                .attachments
                .iter()
                .any(|existing| existing.path == restored.path)
        });
        attachments.append(&mut thread.composer.attachments);
        thread.composer.attachments = attachments;
        if shown {
            let current = self.window.global::<ComposerState>().get_text();
            if current.trim().is_empty() {
                let cursor = draft.text.len();
                self.composer_set_text(draft.text, cursor);
            }
        } else if thread.composer.text.trim().is_empty() {
            thread.composer.text = draft.text;
        }
    }

    /// Puts `text` back into the empty composer (a command that could not
    /// run keeps what the user typed).
    fn composer_restore_text(&mut self, text: String) {
        let current = self.window.global::<ComposerState>().get_text();
        if current.trim().is_empty() {
            let cursor = text.len();
            self.composer_set_text(text, cursor);
        }
    }

    /// `/plan <message>`: enters Plan mode (without toggling it off) and
    /// sends the message, like the TUI.
    fn composer_plan_message(&mut self, index: usize, message: String) {
        let typed = format!("/plan {message}");
        if self.composer_shared.plan_mask.is_none() {
            self.toast("Plan mode is not available with this server");
            self.composer_restore_text(typed);
            return;
        }
        if !self.composer_switch_plan(index, /*enable*/ Some(true)) {
            self.composer_restore_text(typed);
            return;
        }
        self.composer_carry_mode_on_next_turn(index);
        if !self.composer_send_message(index, &message) {
            self.composer_restore_text(typed);
        }
    }

    /// Runs a palette command for tab `index`. `args` is non-empty only for
    /// commands that take arguments ([`SlashCommand::takes_args`]).
    fn composer_run_slash(&mut self, index: usize, command: SlashCommand, args: String) {
        match command {
            SlashCommand::Model => self.composer_open_picker("model"),
            SlashCommand::Permissions => self.composer_open_picker("permissions"),
            SlashCommand::Approve => self.composer_approve_denial(index),
            SlashCommand::Plan if !args.is_empty() => self.composer_plan_message(index, args),
            SlashCommand::Plan => {
                if self.composer_shared.plan_mask.is_some() {
                    self.composer_toggle_plan();
                } else {
                    self.toast("Plan mode is not available with this server");
                }
            }
            SlashCommand::Review => self.review_open(index, &args),
            SlashCommand::Rename => {
                self.thread_tab_action(index, "rename");
                if !args.is_empty() {
                    self.window
                        .global::<AppState>()
                        .set_dialog_input(args.into());
                }
            }
            SlashCommand::New => {
                if let Some(cwd) = self.thread_tab(index).map(|thread| thread.cwd.clone()) {
                    self.start_thread_in_folder(cwd);
                    if !args.is_empty()
                        && let Some(created) = self.active_thread_index()
                    {
                        self.composer_name_pending_thread(self.tabs[created].id, args, true);
                    }
                } else {
                    self.start_folderless_thread();
                }
            }
            SlashCommand::Resume => {
                self.sidebar_focus_search();
                if !args.is_empty() {
                    let sidebar = self.window.global::<SidebarState>();
                    sidebar.set_search_text(args.as_str().into());
                    sidebar.invoke_search_edited(args.into());
                }
            }
            SlashCommand::Fork => {
                let source = self.tabs[index].id;
                self.thread_tab_action(index, "fork");
                // `fork_tab` opens and activates the new tab right away.
                if !args.is_empty()
                    && let Some(fork) = self.active_thread_index()
                    && self.tabs[fork].id != source
                {
                    let tab_id = self.tabs[fork].id;
                    if let Some(thread) = self.thread_tab_mut(fork) {
                        thread.name = Some(args.clone());
                    }
                    self.refresh_tabs();
                    self.composer_name_pending_thread(tab_id, args, /*claimed*/ true);
                }
            }
            SlashCommand::Side => self.side_chat_start(index, (!args.is_empty()).then_some(args)),
            SlashCommand::Recap => self.thread_tab_action(index, "recap"),
            SlashCommand::Worktree => self.thread_tab_action(index, "worktree"),
            SlashCommand::Ps => self.info_show_terminals(index),
            SlashCommand::Stop => self.info_stop_all_terminals(index),
            SlashCommand::Compact => self.thread_tab_action(index, "compact"),
            SlashCommand::Init => self.thread_tab_action(index, "init"),
            SlashCommand::Export => self.thread_tab_action(index, "export"),
            SlashCommand::Archive => self.thread_tab_action(index, "archive"),
            SlashCommand::Diff => self.info_open_diff(index),
            SlashCommand::Mention => {
                let current = self.window.global::<ComposerState>().get_text().to_string();
                let cursor = text::floor_char_boundary(&current, self.composer_shared.cursor);
                let needs_space = current[..cursor]
                    .chars()
                    .next_back()
                    .is_some_and(|c| !c.is_whitespace());
                self.composer_insert_text(if needs_space { " @" } else { "@" });
                let updated = self.window.global::<ComposerState>().get_text().to_string();
                let cursor = self.composer_shared.cursor;
                self.composer_input_changed(&updated, cursor);
            }
            SlashCommand::Copy => match self.transcript_last_agent_message(index) {
                Some(reply) => self.copy_to_clipboard(&reply),
                None => self.toast("There is no reply to copy yet"),
            },
            SlashCommand::Status => self.info_show_status(index),
            SlashCommand::Settings => self.open_settings(Some("common")),
            SlashCommand::Mcp => self.open_settings(Some("mcp")),
            SlashCommand::Skills => self.open_settings(Some("skills")),
            SlashCommand::Plugins => self.open_settings(Some("plugins")),
            SlashCommand::Hooks => self.open_settings(Some("hooks")),
            SlashCommand::Experimental => self.open_settings(Some("features")),
            SlashCommand::Memories => self.open_settings(Some("memories")),
            SlashCommand::Theme => self.open_settings(Some("appearance")),
            SlashCommand::DebugConfig => self.open_settings(Some("diagnostics")),
            SlashCommand::Logout => self.open_settings(Some("account")),
            SlashCommand::Quit => {
                self.window
                    .global::<AppState>()
                    .invoke_menu_action("quit".into());
            }
        }
    }

    /// Opens one of the toolbar pickers from code (`/model`, `/permissions`).
    fn composer_open_picker(&mut self, picker: &str) {
        let empty = match picker {
            "model" => self.composer_shared.models_model.row_count() == 0,
            "effort" => self.composer_shared.efforts_model.row_count() == 0,
            _ => false,
        };
        if empty {
            self.toast(match picker {
                "model" => "The model list is not available",
                _ => "This model has no reasoning effort options",
            });
            return;
        }
        let shared = &mut self.composer_shared;
        shared.picker_serial = shared.picker_serial.wrapping_add(1);
        let state = self.window.global::<ComposerState>();
        state.set_picker_request(picker.into());
        state.set_picker_serial(shared.picker_serial);
    }

    /// Replaces the visible text, moves the cursor and focuses the input.
    fn composer_set_text(&mut self, text: String, cursor: usize) {
        let state = self.window.global::<ComposerState>();
        let shared = &mut self.composer_shared;
        shared.cursor = cursor;
        shared.cursor_serial = shared.cursor_serial.wrapping_add(1);
        shared.focus_serial = shared.focus_serial.wrapping_add(1);
        state.set_text(text.into());
        state.set_cursor_request(i32::try_from(cursor).unwrap_or(i32::MAX));
        state.set_cursor_serial(shared.cursor_serial);
        state.set_focus_serial(shared.focus_serial);
    }

    /// Moves keyboard focus to the input.
    pub(crate) fn composer_focus(&mut self) {
        let shared = &mut self.composer_shared;
        shared.focus_serial = shared.focus_serial.wrapping_add(1);
        self.window
            .global::<ComposerState>()
            .set_focus_serial(shared.focus_serial);
    }

    /// Pushes the active thread's state (phase, settings, attachments) into
    /// the view. Leaves the text alone.
    pub(crate) fn composer_refresh(&mut self) {
        let state = self.window.global::<ComposerState>();
        let enter_sends = self.prefs.enter_sends;
        let busy_input = self.prefs.busy_input;
        state.set_enter_sends(enter_sends);
        let thread = self
            .active_thread_index()
            .and_then(|index| self.thread_tab(index));
        let Some(thread) = thread else {
            state.set_enabled(false);
            state.set_busy(false);
            self.composer_shared.attachments_model.set_vec(Vec::new());
            return;
        };
        let enabled = !matches!(thread.phase, ThreadPhase::Closed | ThreadPhase::Error);
        let busy = thread.is_busy();
        state.set_enabled(enabled);
        state.set_busy(busy);
        state.set_placeholder(placeholder(thread.phase, busy_input, enter_sends).into());
        state.set_send_label(
            match (busy, busy_input) {
                (true, BusyInput::Steer) => "Steer",
                (true, BusyInput::Queue) => "Queue",
                (false, _) => "Send",
            }
            .into(),
        );

        let models = &self.composer_shared.models;
        let model = toolbar::effective_model(thread);
        let effort = toolbar::effective_effort(thread);
        let approval = toolbar::effective_approval(thread);
        let sandbox = toolbar::effective_sandbox(thread);
        let catalog_model = model
            .as_deref()
            .and_then(|slug| presets::find_model(models, slug));

        state.set_model_label(presets::model_label(models, model.as_deref()).into());
        let mut model_choices: Vec<ComposerChoice> = models
            .iter()
            .filter(|candidate| !candidate.hidden || Some(&candidate.model) == model.as_ref())
            .map(|candidate| ComposerChoice {
                id: candidate.model.as_str().into(),
                label: candidate.display_name.as_str().into(),
                detail: candidate
                    .description
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .into(),
                selected: Some(&candidate.model) == model.as_ref(),
            })
            .collect();
        if let Some(model) = model.as_deref()
            && catalog_model.is_none()
        {
            model_choices.insert(
                0,
                ComposerChoice {
                    id: model.into(),
                    label: model.into(),
                    detail: "Current model".into(),
                    selected: true,
                },
            );
        }
        set_choices(&self.composer_shared.models_model, model_choices);

        let effort_label = effort
            .as_ref()
            .or_else(|| catalog_model.map(|model| &model.default_reasoning_effort))
            .map(presets::effort_label)
            .unwrap_or_else(|| "Default effort".to_string());
        state.set_effort_label(effort_label.into());
        state.set_effort_visible(effort.is_some() || catalog_model.is_some());
        let effort_choices: Vec<ComposerChoice> = catalog_model
            .map(|catalog| {
                let current = effort
                    .clone()
                    .unwrap_or_else(|| catalog.default_reasoning_effort.clone());
                catalog
                    .supported_reasoning_efforts
                    .iter()
                    .map(|option| ComposerChoice {
                        id: option.reasoning_effort.as_str().into(),
                        label: presets::effort_label(&option.reasoning_effort).into(),
                        detail: option.description.as_str().into(),
                        selected: option.reasoning_effort == current,
                    })
                    .collect()
            })
            .unwrap_or_default();
        set_choices(&self.composer_shared.efforts_model, effort_choices);

        let reviewer = toolbar::effective_reviewer(thread);
        state.set_permission_label(
            presets::permission_label(approval.as_ref(), sandbox.as_ref(), reviewer).into(),
        );
        let current_preset =
            PermissionPreset::matching(approval.as_ref(), sandbox.as_ref(), reviewer);
        let auto_review_available = toolbar::auto_review_allowed(
            self.composer_shared.auto_review_feature,
            self.settings.requirements.as_ref(),
        );
        set_choices(
            &self.composer_shared.permissions_model,
            PermissionPreset::offered(auto_review_available, current_preset)
                .into_iter()
                .map(|preset| ComposerChoice {
                    id: preset.id().into(),
                    label: preset.label().into(),
                    detail: preset.description().into(),
                    selected: Some(preset) == current_preset,
                })
                .collect(),
        );

        state.set_plan_available(self.composer_shared.plan_mask.is_some());
        state.set_plan_enabled(enabled && model.is_some());
        state.set_plan_mode(thread.composer.mode == ModeKind::Plan);
        state.set_context_label(
            thread
                .composer
                .context_left
                .map(|percent| format!("{percent}% context left"))
                .unwrap_or_default()
                .into(),
        );
        state.set_images_allowed(presets::model_supports_images(models, model.as_deref()));
        let chips: Vec<ComposerChip> = thread.composer.attachments.iter().map(chip).collect();
        let attachments_model = self.composer_shared.attachments_model.clone();
        if attachments_model.row_count() != chips.len()
            || chips
                .iter()
                .enumerate()
                .any(|(row, chip)| attachments_model.row_data(row).as_ref() != Some(chip))
        {
            attachments_model.set_vec(chips);
        }
    }

    /// Scripted composer actions for `CODEX_GUI_AUTOMATION` (see
    /// `automation::Step::Composer`), plus `["drop", path...]` and
    /// `["drop-hover", "true"|"false"]` to stand in for OS file drops.
    pub(crate) fn composer_automation(&mut self, args: &[String]) {
        let arg = |index: usize| args.get(index).map(String::as_str).unwrap_or_default();
        match arg(0) {
            "type" => {
                // `["type", text, cursor?]`: the cursor defaults to the end.
                let text = arg(1);
                let cursor = arg(2).parse().map_or(text.len(), |cursor: usize| {
                    text::floor_char_boundary(text, cursor)
                });
                self.composer_set_text(text.to_string(), cursor);
                self.composer_input_changed(text, cursor);
            }
            "attach" => {
                if let Some(index) = self.active_thread_index() {
                    let tab_id = self.tabs[index].id;
                    self.composer_attach_to_tab(tab_id, vec![PathBuf::from(arg(1))]);
                }
            }
            "accept" => self.composer_accept_suggestion(arg(1).parse().unwrap_or_default()),
            "denial" => {
                // Stands in for an auto-review denial notification:
                // `["denial", review_id, command, rationale]`.
                if let Some(index) = self.active_thread_index() {
                    let thread_id = self
                        .thread_tab(index)
                        .and_then(|thread| thread.thread_id.clone())
                        .unwrap_or_default();
                    let cwd = self
                        .thread_tab(index)
                        .map(|thread| thread.cwd.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let review = serde_json::json!({
                        "threadId": thread_id,
                        "turnId": "automation",
                        "startedAtMs": 0,
                        "completedAtMs": 0,
                        "reviewId": arg(1),
                        "targetItemId": null,
                        "decisionSource": "agent",
                        "review": {
                            "status": "denied",
                            "riskLevel": "high",
                            "userAuthorization": "low",
                            "rationale": arg(3),
                        },
                        "action": {
                            "type": "command",
                            "source": "shell",
                            "command": arg(2),
                            "cwd": cwd,
                        },
                    });
                    match serde_json::from_value(review) {
                        Ok(review) => self.composer_on_guardian_review(&review),
                        Err(err) => tracing::warn!(%err, "bad automation denial"),
                    }
                }
            }
            "choose" => self.composer_choose(arg(1), arg(2)),
            "open" => self.composer_open_picker(arg(1)),
            "plan" => self.composer_toggle_plan(),
            "send" => self.composer_submit(),
            "drop" => {
                let paths = args.iter().skip(1).map(PathBuf::from).collect();
                self.composer_drop_paths(paths);
            }
            "drop-hover" => self.composer_set_drop_hover(arg(1) == "true"),
            other => tracing::warn!(action = other, "unknown composer automation action"),
        }
    }
}

/// Placeholder text for the input.
fn placeholder(phase: ThreadPhase, busy_input: BusyInput, enter_sends: bool) -> String {
    match phase {
        ThreadPhase::Closed => "This thread is closed.".to_string(),
        ThreadPhase::Error => "This thread is unavailable.".to_string(),
        ThreadPhase::Starting => "Starting… you can start typing.".to_string(),
        ThreadPhase::Running | ThreadPhase::WaitingOnUser => match busy_input {
            BusyInput::Steer => "Working… type to steer, Esc to stop".to_string(),
            BusyInput::Queue => "Working… type to queue a message, Esc to stop".to_string(),
        },
        ThreadPhase::Idle => {
            if enter_sends {
                "Ask Codex anything. @ to mention files, / for commands".to_string()
            } else {
                let send = if cfg!(target_os = "macos") {
                    "⌘↩"
                } else {
                    "Ctrl+Enter"
                };
                format!("Ask Codex anything. @ files, / commands, {send} to send")
            }
        }
    }
}

fn chip(attachment: &Attachment) -> ComposerChip {
    let thumb = attachment
        .thumbnail
        .clone()
        .map(slint::Image::from_rgba8)
        .unwrap_or_default();
    ComposerChip {
        name: attachment.file_name().into(),
        detail: attachment
            .dimensions
            .map(|(width, height)| format!("{width}×{height}"))
            .unwrap_or_default()
            .into(),
        has_thumb: attachment.thumbnail.is_some(),
        thumb,
        path: SharedString::from(attachment.path.to_string_lossy().as_ref()),
    }
}

/// Updates a choice model only where rows changed (keeps open popups stable).
fn set_choices(model: &VecModel<ComposerChoice>, choices: Vec<ComposerChoice>) {
    if model.row_count() != choices.len() {
        model.set_vec(choices);
        return;
    }
    for (row, choice) in choices.into_iter().enumerate() {
        if model.row_data(row).as_ref() != Some(&choice) {
            model.set_row_data(row, choice);
        }
    }
}

/// Whether `thread` can take image attachments with its current model.
fn thread_accepts_images(shared: &ComposerShared, thread: &ThreadTab) -> bool {
    presets::model_supports_images(&shared.models, toolbar::effective_model(thread).as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn placeholder_reflects_state() {
        assert_eq!(
            placeholder(ThreadPhase::Running, BusyInput::Steer, true),
            "Working… type to steer, Esc to stop"
        );
        assert_eq!(
            placeholder(ThreadPhase::WaitingOnUser, BusyInput::Queue, true),
            "Working… type to queue a message, Esc to stop"
        );
        assert_eq!(
            placeholder(ThreadPhase::Closed, BusyInput::Steer, true),
            "This thread is closed."
        );
        assert!(placeholder(ThreadPhase::Idle, BusyInput::Steer, false).contains("to send"));
    }
}
