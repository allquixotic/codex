//! File viewer and diff viewer tabs (GUI.md §5.8).
//!
//! A file tab shows one file as selectable, read-only monospace text with a
//! line-number gutter, find, go to line, wrap, and live reload. A text tab
//! is the same viewer over text that has no file (a message or a whole
//! thread opened "as text", so any part of it can be selected). A diff tab
//! shows a unified diff (for example the aggregated turn diff) as a
//! virtualized list with per-file stats, collapsing, and intra-line
//! highlights.
//!
//! Only the active tab's state lives in the `FilesState` global; each tab
//! keeps its own state (text, scroll position, find query) in [`FileTab`] and
//! [`AppController::files_show`] swaps it in.

mod diff;
mod diff_view;
pub(crate) mod external;
mod find;
mod text;
mod viewer;

use std::path::Path;
use std::path::PathBuf;

use codex_app_server_protocol::FileUpdateChange;
use codex_app_server_protocol::ServerNotification;
use slint::ComponentHandle;

use crate::app::AppController;
use crate::app::Tab;
use crate::app::TabId;
use crate::app::TabKind;
use crate::ui::FilesState;

use self::diff_view::DiffDoc;
use self::viewer::TextDoc;

/// A file or diff tab.
pub(crate) struct FileTab {
    kind: FileTabKind,
}

enum FileTabKind {
    Text(Box<TextDoc>),
    Diff(Box<DiffDoc>),
}

impl FileTab {
    pub(crate) fn title(&self) -> String {
        match &self.kind {
            FileTabKind::Text(doc) => doc.title(),
            FileTabKind::Diff(doc) => doc.title.clone(),
        }
    }

    pub(crate) fn tooltip(&self) -> String {
        match &self.kind {
            FileTabKind::Text(doc) => match &doc.memory {
                Some(memory) => memory.title.clone(),
                None => doc.path.display().to_string(),
            },
            FileTabKind::Diff(doc) => match &doc.base {
                Some(base) => format!("{} — {}", doc.title, base.display()),
                None => doc.title.clone(),
            },
        }
    }
}

/// `FilesState.tab-id` value for a tab id.
fn tab_key(id: TabId) -> i32 {
    i32::try_from(id).unwrap_or(-1)
}

fn text_doc_ref(tabs: &[Tab], index: usize) -> Option<&TextDoc> {
    match &tabs.get(index)?.kind {
        TabKind::File(file) => match &file.kind {
            FileTabKind::Text(doc) => Some(doc),
            FileTabKind::Diff(_) => None,
        },
        _ => None,
    }
}

fn text_doc_at(tabs: &mut [Tab], index: usize) -> Option<&mut TextDoc> {
    match &mut tabs.get_mut(index)?.kind {
        TabKind::File(file) => match &mut file.kind {
            FileTabKind::Text(doc) => Some(doc),
            FileTabKind::Diff(_) => None,
        },
        _ => None,
    }
}

fn diff_doc_at(tabs: &mut [Tab], index: usize) -> Option<&mut DiffDoc> {
    match &mut tabs.get_mut(index)?.kind {
        TabKind::File(file) => match &mut file.kind {
            FileTabKind::Diff(doc) => Some(doc),
            FileTabKind::Text(_) => None,
        },
        _ => None,
    }
}

impl AppController {
    pub(crate) fn files_bind(&mut self) {
        let state = self.window.global::<FilesState>();
        state.on_toggle_wrap(|| crate::ui_thread::with_app(AppController::files_toggle_wrap));
        state.on_open_find(|| crate::ui_thread::with_app(AppController::files_open_find));
        state.on_find_edited(|query| {
            let query = query.to_string();
            crate::ui_thread::with_app(move |app| app.files_find_edited(query));
        });
        state.on_find_next(|| {
            crate::ui_thread::with_app(|app| app.files_find_step(/*forward*/ true))
        });
        state.on_find_prev(|| {
            crate::ui_thread::with_app(|app| app.files_find_step(/*forward*/ false))
        });
        state.on_close_find(|| crate::ui_thread::with_app(AppController::files_close_find));
        state.on_go_to_line(|| crate::ui_thread::with_app(AppController::files_prompt_go_to_line));
        state.on_copy_path(|| crate::ui_thread::with_app(AppController::files_copy_path));
        state.on_reveal(|| {
            crate::ui_thread::with_app(|app| {
                app.files_open_external(external::ExternalAction::Reveal)
            });
        });
        state.on_open_externally(|| {
            crate::ui_thread::with_app(|app| {
                app.files_open_external(external::ExternalAction::Open)
            });
        });
        state.on_reload(|| crate::ui_thread::with_app(AppController::files_reload_active));
        state.on_load_more(|| crate::ui_thread::with_app(AppController::files_load_more));
        state.on_show_as_text(|| crate::ui_thread::with_app(AppController::files_show_as_text));
        state.on_cursor_moved(|offset| {
            crate::ui_thread::with_app(move |app| app.files_cursor_moved(offset));
        });
        state.on_diff_toggle_file(|file| {
            crate::ui_thread::with_app(move |app| app.files_diff_toggle(file));
        });
        state.on_diff_set_collapsed(|collapsed| {
            crate::ui_thread::with_app(move |app| app.files_diff_set_collapsed(collapsed));
        });
        state.on_diff_open_file(|file| {
            crate::ui_thread::with_app(move |app| app.files_diff_open_file(file));
        });
        state.on_copy_diff(|| crate::ui_thread::with_app(AppController::files_copy_diff));
        state.on_copy_all(|| crate::ui_thread::with_app(AppController::files_copy_all));
        state.on_save_text_as(|| crate::ui_thread::with_app(AppController::files_save_text_as));
        state.on_viewport_changed(|first, last| {
            crate::ui_thread::with_app(move |app| app.files_viewport_changed(first, last));
        });
    }

    /// Loads the active file tab into the `FilesState` global.
    pub(crate) fn files_show(&mut self) {
        let Some(index) = self.files_active_index() else {
            return;
        };
        let switching = !self.files_is_shown(index);
        if switching {
            self.files_save_shown_view();
        }
        if text_doc_at(&mut self.tabs, index).is_some() {
            self.files_push_text(index, switching);
            // A "go to line" that finished loading while another tab was
            // shown is applied now.
            self.files_apply_pending_line(index);
            self.files_check_fresh(index);
        } else {
            self.files_push_diff(index, switching);
        }
    }

    /// Opens `path` in a file tab, or focuses the tab already showing it,
    /// optionally scrolled to `line` (1-based).
    pub(crate) fn open_file_tab(&mut self, path: PathBuf, line: Option<usize>) {
        let path = self.files_absolute_path(path);
        if let Some(index) = self.files_tab_for_path(&path) {
            self.activate_tab(index);
            if let Some(line) = line {
                self.files_go_to_line(index, line);
            }
            return;
        }
        let doc = TextDoc::new(path, line);
        let index = self.replace_new_tab_page_or_push(TabKind::File(Box::new(FileTab {
            kind: FileTabKind::Text(Box::new(doc)),
        })));
        self.files_load(index, viewer::LoadKind::Initial);
    }

    /// Opens read-only, selectable text that is not backed by a file (for
    /// example a whole message or thread as Markdown) in the file viewer:
    /// find, wrap, go to line, copy all, and save, without file watching.
    /// The tab is titled "<title> (text)"; a text tab with the same title is
    /// updated and focused instead of opening another one.
    pub(crate) fn open_text_tab(&mut self, title: String, text: String) {
        let existing = (0..self.tabs.len()).find(|&index| {
            text_doc_ref(&self.tabs, index)
                .and_then(|doc| doc.memory.as_ref())
                .is_some_and(|memory| memory.title == title)
        });
        let index = match existing {
            Some(index) => {
                // Keep the view state of whatever the viewer holds, then show
                // the new text from the top, like a new tab.
                self.files_save_shown_view();
                self.window.global::<FilesState>().set_tab_id(-1);
                if let Some(doc) = text_doc_at(&mut self.tabs, index) {
                    doc.replace_memory(text);
                }
                self.activate_tab(index);
                index
            }
            None => self.replace_new_tab_page_or_push(TabKind::File(Box::new(FileTab {
                kind: FileTabKind::Text(Box::new(TextDoc::in_memory(title, text))),
            }))),
        };
        self.files_load(index, viewer::LoadKind::Initial);
    }

    /// Opens a unified diff in a read-only diff tab. A diff tab with the same
    /// title is updated and focused instead of opening another one.
    pub(crate) fn open_diff_tab(&mut self, title: String, unified_diff: String) {
        self.open_diff_tab_in(title, unified_diff, /*base*/ None);
    }

    /// Like [`AppController::open_diff_tab`], resolving the diff's relative
    /// paths against `base` for "Open" links. Defaults to the active
    /// thread's folder.
    pub(crate) fn open_diff_tab_in(
        &mut self,
        title: String,
        unified_diff: String,
        base: Option<PathBuf>,
    ) {
        let base = base.or_else(|| {
            self.active_thread_index()
                .and_then(|index| self.thread_tab(index))
                .map(|thread| thread.cwd.clone())
                .filter(|cwd| !cwd.as_os_str().is_empty())
        });
        let existing = self.tabs.iter().position(|tab| match &tab.kind {
            TabKind::File(file) => {
                matches!(&file.kind, FileTabKind::Diff(doc) if doc.title == title)
            }
            _ => false,
        });
        let index = match existing {
            Some(index) => {
                if let Some(doc) = diff_doc_at(&mut self.tabs, index) {
                    doc.replace(unified_diff, base);
                }
                self.activate_tab(index);
                index
            }
            None => self.replace_new_tab_page_or_push(TabKind::File(Box::new(FileTab {
                kind: FileTabKind::Diff(Box::new(DiffDoc::new(title, unified_diff, base))),
            }))),
        };
        self.files_parse_diff(index);
    }

    /// Opens `FileChange` items (adds and deletes carry whole files) in a
    /// diff tab.
    #[allow(
        dead_code,
        reason = "entry point for patch cards in the transcript and approvals"
    )]
    pub(crate) fn open_file_changes_tab(
        &mut self,
        title: String,
        changes: &[FileUpdateChange],
        base: Option<PathBuf>,
    ) {
        let unified = diff::file_changes_to_unified_diff(changes, base.as_deref());
        self.open_diff_tab_in(title, unified, base);
    }

    /// Shows a file picker and opens the chosen file.
    pub(crate) fn files_pick_and_open(&mut self) {
        let mut dialog = rfd::AsyncFileDialog::new().set_title("Open file");
        if let Some(folder) = self
            .active_thread_index()
            .and_then(|index| self.thread_tab(index))
            .map(|thread| thread.cwd.clone())
            .filter(|cwd| cwd.is_absolute())
        {
            dialog = dialog.set_directory(folder);
        }
        let future = dialog.pick_file();
        let spawned = slint::spawn_local(async move {
            if let Some(file) = future.await {
                let path = file.path().to_path_buf();
                crate::ui_thread::with_app(move |app| app.open_file_tab(path, None));
            }
        });
        if let Err(err) = spawned {
            tracing::warn!(%err, "could not open file picker");
        }
    }

    pub(crate) fn files_on_notification(&mut self, notification: &ServerNotification) {
        if let ServerNotification::FsChanged(changed) = notification {
            self.files_on_fs_changed(&changed.watch_id);
        }
    }

    pub(crate) fn files_on_tab_closed(&mut self, tab: FileTab) {
        if let FileTabKind::Text(doc) = tab.kind {
            self.files_unwatch(*doc);
        }
    }

    /// Re-registers file watches after the embedded server restarted
    /// (watches are scoped to the old connection) and re-checks the active
    /// file for changes made meanwhile.
    pub(crate) fn files_on_server_ready(&mut self, restarted: bool) {
        if restarted {
            self.files_rewatch_all();
        }
    }

    /// Runs a scripted file-viewer command (automation only): `find-open`,
    /// `find:<text>`, `find-next`, `find-prev`, `find-close`, `goto-prompt`,
    /// `goto:<line>`, `wrap`,
    /// `reload`, `load-more`, `show-as-text`, `toggle:<file>`,
    /// `collapse-all`, `expand-all`, `scroll:<y>`, `copy-all`,
    /// `text:<title>|<text>` (opens a text tab; `\n` for newlines),
    /// `open-externally`.
    pub(crate) fn files_automation(&mut self, command: &str) {
        let (name, argument) = command.split_once(':').unwrap_or((command, ""));
        match name {
            "find" => {
                self.files_open_find();
                self.window
                    .global::<FilesState>()
                    .set_find_query(argument.into());
                self.files_find_edited(argument.to_string());
            }
            "find-open" => self.files_open_find(),
            "find-next" => self.files_find_step(/*forward*/ true),
            "find-prev" => self.files_find_step(/*forward*/ false),
            "find-close" => self.files_close_find(),
            "goto-prompt" => self.files_prompt_go_to_line(),
            "goto" => {
                if let (Some(index), Ok(line)) =
                    (self.files_active_index(), argument.trim().parse())
                {
                    self.files_go_to_line(index, line);
                }
            }
            "wrap" => self.files_toggle_wrap(),
            "reload" => self.files_reload_active(),
            "load-more" => self.files_load_more(),
            "show-as-text" => self.files_show_as_text(),
            "toggle" => {
                if let Ok(file) = argument.trim().parse() {
                    self.files_diff_toggle(file);
                }
            }
            "collapse-all" => self.files_diff_set_collapsed(/*collapsed*/ true),
            "expand-all" => self.files_diff_set_collapsed(/*collapsed*/ false),
            "scroll" => {
                if let Ok(y) = argument.trim().parse::<f32>() {
                    self.window.global::<FilesState>().set_scroll_y(-y.abs());
                }
            }
            "copy-all" => self.files_copy_all(),
            "open-externally" => self.files_open_external(external::ExternalAction::Open),
            "text" => {
                let (title, text) = argument.split_once('|').unwrap_or((argument, ""));
                self.open_text_tab(title.to_string(), text.replace("\\n", "\n"));
            }
            other => tracing::warn!(command = other, "unknown file viewer automation command"),
        }
    }

    // ----- helpers shared by the text and diff views ---------------------

    /// Active tab index when it is a file or diff tab.
    fn files_active_index(&self) -> Option<usize> {
        self.active.filter(|&index| {
            self.tabs
                .get(index)
                .is_some_and(|tab| matches!(tab.kind, TabKind::File(_)))
        })
    }

    /// Whether the `FilesState` global currently holds tab `index`.
    fn files_is_shown(&self, index: usize) -> bool {
        self.active == Some(index)
            && self.tabs.get(index).is_some_and(|tab| {
                self.window.global::<FilesState>().get_tab_id() == tab_key(tab.id)
            })
    }

    /// Copies the view state of the tab currently in the global (scroll
    /// position, find query) back into that tab before another tab is shown.
    fn files_save_shown_view(&mut self) {
        let state = self.window.global::<FilesState>();
        let shown = state.get_tab_id();
        let scroll = (state.get_scroll_x(), state.get_scroll_y());
        let query = state.get_find_query().to_string();
        let Some(index) = self.tabs.iter().position(|tab| tab_key(tab.id) == shown) else {
            return;
        };
        if let Some(doc) = text_doc_at(&mut self.tabs, index) {
            doc.save_view(scroll, query);
        } else if let Some(doc) = diff_doc_at(&mut self.tabs, index) {
            doc.scroll = scroll;
        }
    }

    /// Resolves a possibly relative path against the active thread's folder
    /// (or the process working directory) and normalizes it lexically.
    fn files_absolute_path(&self, path: PathBuf) -> PathBuf {
        let absolute = if path.is_absolute() {
            path
        } else {
            let base = self
                .active_thread_index()
                .and_then(|index| self.thread_tab(index))
                .map(|thread| thread.cwd.clone())
                .filter(|cwd| cwd.is_absolute())
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_default();
            base.join(path)
        };
        text::normalize_lexically(&absolute)
    }

    fn files_tab_for_path(&self, path: &Path) -> Option<usize> {
        self.tabs.iter().position(|tab| match &tab.kind {
            TabKind::File(file) => match &file.kind {
                FileTabKind::Text(doc) => doc.shows_path(path),
                FileTabKind::Diff(_) => false,
            },
            _ => false,
        })
    }
}
