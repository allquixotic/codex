//! Files dropped onto the window from the OS file manager.
//!
//! Slint 1.18's `DropArea` only receives drags that start in the app: the
//! winit backend does not turn the OS's file drops into `DropArea` events,
//! and the `data-transfer` payload has no stable Rust API. winit itself
//! reports them as `HoveredFile` / `DroppedFile` window events, which reach
//! us through the window event filter of `slint::winit_030` (one event per
//! file, no position). Drops therefore go to the composer of the active
//! thread tab wherever they land: images are attached, other files and
//! folders are inserted as (quoted when needed) paths relative to the
//! thread's folder. Wayland does not report file drops to winit.

use std::path::Path;
use std::path::PathBuf;

use slint::ComponentHandle;
use slint::winit_030::EventResult;
use slint::winit_030::WinitWindowAccessor;
use slint::winit_030::winit::event::WindowEvent;

use super::attachments::is_image_path;
use super::text;
use crate::app::AppController;
use crate::ui::ComposerState;

/// How a dropped path is used.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DropAction {
    Attach(PathBuf),
    Insert(String),
}

/// Decides what to do with one dropped path. `is_dir` is whether it is a
/// folder; `images_allowed` whether the model accepts images.
pub(crate) fn drop_action(
    path: &Path,
    cwd: &Path,
    is_dir: bool,
    images_allowed: bool,
) -> DropAction {
    if !is_dir && images_allowed && is_image_path(path) {
        return DropAction::Attach(path.to_path_buf());
    }
    let mut shown = match path.strip_prefix(cwd) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.to_string_lossy().into_owned(),
        _ => path.to_string_lossy().into_owned(),
    };
    if is_dir && !shown.ends_with(std::path::MAIN_SEPARATOR) {
        shown.push(std::path::MAIN_SEPARATOR);
    }
    DropAction::Insert(text::quote_path(&shown))
}

/// Text to insert for `inserted` at the end of `before`: separated from
/// the preceding text by a space, and followed by one.
pub(crate) fn insertion_text(before: &str, inserted: &str) -> String {
    let needs_space = before
        .chars()
        .next_back()
        .is_some_and(|c| !c.is_whitespace());
    format!("{}{inserted} ", if needs_space { " " } else { "" })
}

impl AppController {
    /// Routes the OS's file drag-and-drop events to the composer.
    pub(super) fn composer_install_drop_handler(&self) {
        // The filter is the window's only one; nothing else in the app
        // needs raw winit events.
        self.window
            .window()
            .on_winit_window_event(|_window, event| match event {
                WindowEvent::HoveredFile(_) => {
                    crate::ui_thread::with_app(|app| app.composer_set_drop_hover(true));
                    EventResult::Propagate
                }
                WindowEvent::HoveredFileCancelled => {
                    crate::ui_thread::with_app(|app| app.composer_set_drop_hover(false));
                    EventResult::Propagate
                }
                WindowEvent::DroppedFile(path) => {
                    let path = path.clone();
                    crate::ui_thread::with_app(move |app| {
                        app.composer_set_drop_hover(false);
                        app.composer_drop_paths(vec![path]);
                    });
                    EventResult::Propagate
                }
                _ => EventResult::Propagate,
            });
    }

    pub(super) fn composer_set_drop_hover(&self, hover: bool) {
        let accepts = hover && self.active_thread_index().is_some();
        self.window
            .global::<ComposerState>()
            .set_drop_hover(accepts);
    }

    /// Attaches dropped images and inserts the paths of other files.
    pub(super) fn composer_drop_paths(&mut self, paths: Vec<PathBuf>) {
        let Some(index) = self.active_thread_index() else {
            if !paths.is_empty() {
                self.toast("Open a thread tab to drop files into its message");
            }
            return;
        };
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        if !self.window.global::<ComposerState>().get_enabled() {
            return;
        }
        let images_allowed = super::thread_accepts_images(&self.composer_shared, thread);
        let cwd = thread.cwd.clone();
        let mut attachments = Vec::new();
        let mut inserted = Vec::new();
        for path in paths {
            // A stat per dropped file; drops are user-paced and few.
            let is_dir = path.is_dir();
            match drop_action(&path, &cwd, is_dir, images_allowed) {
                DropAction::Attach(path) => attachments.push(path),
                DropAction::Insert(text) => inserted.push(text),
            }
        }
        if !attachments.is_empty() {
            let tab_id = self.tabs[index].id;
            self.composer_attach_to_tab(tab_id, attachments);
        }
        if !inserted.is_empty() {
            let current = self.window.global::<ComposerState>().get_text().to_string();
            let cursor = text::floor_char_boundary(&current, self.composer_shared.cursor);
            let insertion = insertion_text(&current[..cursor], &inserted.join(" "));
            self.composer_insert_text(&insertion);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn images_attach_when_the_model_accepts_them() {
        let cwd = Path::new("/work/app");
        assert_eq!(
            drop_action(Path::new("/tmp/shot.PNG"), cwd, false, true),
            DropAction::Attach(PathBuf::from("/tmp/shot.PNG"))
        );
        assert_eq!(
            drop_action(Path::new("/work/app/shot.png"), cwd, false, false),
            DropAction::Insert("shot.png".to_string())
        );
    }

    #[test]
    fn other_files_insert_relative_quoted_paths() {
        let cwd = Path::new("/work/app");
        assert_eq!(
            drop_action(Path::new("/work/app/src/main.rs"), cwd, false, true),
            DropAction::Insert("src/main.rs".to_string())
        );
        assert_eq!(
            drop_action(Path::new("/elsewhere/My Notes.txt"), cwd, false, true),
            DropAction::Insert("\"/elsewhere/My Notes.txt\"".to_string())
        );
        // The folder itself is not a relative path; keep it absolute.
        assert_eq!(
            drop_action(Path::new("/work/app"), cwd, true, true),
            DropAction::Insert(format!("/work/app{}", std::path::MAIN_SEPARATOR))
        );
    }

    #[test]
    fn folders_get_a_trailing_separator() {
        let cwd = Path::new("/work/app");
        let separator = std::path::MAIN_SEPARATOR;
        assert_eq!(
            drop_action(Path::new("/work/app/docs"), cwd, true, true),
            DropAction::Insert(format!("docs{separator}"))
        );
    }

    #[test]
    fn insertion_is_space_separated() {
        assert_eq!(insertion_text("", "a.rs"), "a.rs ");
        assert_eq!(insertion_text("look at", "a.rs"), " a.rs ");
        assert_eq!(insertion_text("look at ", "a.rs b.rs"), "a.rs b.rs ");
        assert_eq!(insertion_text("line\n", "a.rs"), "a.rs ");
    }
}
