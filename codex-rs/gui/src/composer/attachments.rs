//! Image attachments: clipboard images saved as PNG files, file picks, and
//! the small thumbnails shown on attachment chips.
//!
//! Everything here that touches the disk or decodes images runs on Tokio's
//! blocking pool, including reading a bitmap from the clipboard (which
//! decodes it); the UI thread only checks the clipboard for files and text
//! and receives finished results.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use slint::Rgba8Pixel;
use slint::SharedPixelBuffer;

use crate::app::AppController;
use crate::app::TabId;

/// Folder under `$CODEX_HOME` holding pasted clipboard images.
const ATTACHMENT_DIR: &str = "tmp/gui-attachments";
/// Pasted images older than this are deleted when the server starts.
const ATTACHMENT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Edge length of chip thumbnails in physical pixels (2x a 28px chip).
const THUMBNAIL_SIZE: u32 = 56;

/// Extensions the composer treats as images (same list as the TUI).
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// An image attached to the draft.
#[derive(Clone)]
pub(crate) struct Attachment {
    pub(crate) path: PathBuf,
    /// Decoded thumbnail, filled in asynchronously.
    pub(crate) thumbnail: Option<SharedPixelBuffer<Rgba8Pixel>>,
    /// `width×height`, filled in with the thumbnail.
    pub(crate) dimensions: Option<(u32, u32)>,
}

impl Attachment {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            thumbnail: None,
            dimensions: None,
        }
    }

    pub(crate) fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attachment")
            .field("path", &self.path)
            .field("dimensions", &self.dimensions)
            .finish_non_exhaustive()
    }
}

/// Whether `path` has an image extension the model can receive.
pub(crate) fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
}

/// Directory for pasted images.
pub(crate) fn attachment_dir(codex_home: Option<&Path>) -> PathBuf {
    match codex_home {
        Some(home) => home.join(ATTACHMENT_DIR),
        None => std::env::temp_dir().join("codex-gui-attachments"),
    }
}

/// Encodes clipboard RGBA pixels as a PNG in `dir` and returns its path.
pub(crate) fn save_clipboard_png(
    dir: &Path,
    width: usize,
    height: usize,
    rgba: Vec<u8>,
) -> anyhow::Result<PathBuf> {
    let width = u32::try_from(width)?;
    let height = u32::try_from(height)?;
    let image = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| anyhow::anyhow!("the clipboard image has an unexpected size"))?;
    std::fs::create_dir_all(dir)?;
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let path = dir.join(format!("pasted-{stamp}-{}.png", &suffix[..8]));
    image.save_with_format(&path, image::ImageFormat::Png)?;
    Ok(path)
}

/// Decodes `path` and returns a thumbnail plus the original dimensions.
pub(crate) fn load_thumbnail(
    path: &Path,
) -> anyhow::Result<(SharedPixelBuffer<Rgba8Pixel>, (u32, u32))> {
    let decoded = image::ImageReader::open(path)?
        .with_guessed_format()?
        .decode()?;
    let dimensions = (decoded.width(), decoded.height());
    let thumbnail = decoded.thumbnail(THUMBNAIL_SIZE, THUMBNAIL_SIZE).to_rgba8();
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        thumbnail.as_raw(),
        thumbnail.width(),
        thumbnail.height(),
    );
    Ok((buffer, dimensions))
}

/// What Cmd/Ctrl+V does, decided from the cheap clipboard checks only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PastePlan {
    /// Copied image files: attach them.
    AttachFiles,
    /// Copied text wins (spreadsheet cells and rich text also carry a
    /// rendered bitmap): let the text input paste it.
    PasteText,
    /// Nothing else: look for a bitmap in the background. The key is
    /// consumed, which loses nothing since there is no text to paste.
    ReadBitmap,
}

pub(crate) fn paste_plan(has_image_files: bool, has_text: bool) -> PastePlan {
    if has_image_files {
        PastePlan::AttachFiles
    } else if has_text {
        PastePlan::PasteText
    } else {
        PastePlan::ReadBitmap
    }
}

/// Result of reading the clipboard bitmap.
#[derive(Debug)]
pub(crate) enum PastedBitmap {
    /// The clipboard holds no image.
    Nothing,
    /// There is an image but the model does not accept images.
    NotAccepted,
    Saved(PathBuf),
}

/// Reads the clipboard bitmap with its own clipboard handle, so it can run
/// off the UI thread, and saves it as a PNG in `dir`.
fn read_clipboard_bitmap(dir: &Path, accepts_images: bool) -> anyhow::Result<PastedBitmap> {
    let mut clipboard = arboard::Clipboard::new()?;
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(arboard::Error::ContentNotAvailable) => return Ok(PastedBitmap::Nothing),
        Err(err) => return Err(err.into()),
    };
    if !accepts_images {
        return Ok(PastedBitmap::NotAccepted);
    }
    save_clipboard_png(dir, image.width, image.height, image.bytes.into_owned())
        .map(PastedBitmap::Saved)
}

/// Deletes pasted images older than [`ATTACHMENT_MAX_AGE`]. Errors are
/// logged; this is housekeeping only.
pub(crate) fn prune_old_attachments(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let expired = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > ATTACHMENT_MAX_AGE);
        if expired
            && is_image_path(&path)
            && let Err(err) = std::fs::remove_file(&path)
        {
            tracing::debug!(%err, path = %path.display(), "could not prune attachment");
        }
    }
}

impl AppController {
    /// Cmd/Ctrl+V: attaches copied image files or bitmap data. Returns true
    /// when the paste was handled here (so the text input must not paste).
    ///
    /// Answers right away from the cheap checks ([`paste_plan`]); a bitmap
    /// is read, decoded and saved on the blocking pool.
    pub(super) fn composer_paste_image(&mut self) -> bool {
        let Some(index) = self.active_thread_index() else {
            return false;
        };
        let tab_id = self.tabs[index].id;
        let accepts_images = self
            .thread_tab(index)
            .is_some_and(|thread| super::thread_accepts_images(&self.composer_shared, thread));

        let shared = &mut self.composer_shared;
        if shared.clipboard.is_none() {
            shared.clipboard = arboard::Clipboard::new().ok();
        }
        let files: Vec<PathBuf> = shared
            .clipboard
            .as_mut()
            .and_then(|clipboard| clipboard.get().file_list().ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|path| is_image_path(path))
            .collect();
        let has_text = files.is_empty()
            && self
                .clipboard_text()
                .is_some_and(|text| !text.trim().is_empty());
        match paste_plan(!files.is_empty(), has_text) {
            PastePlan::AttachFiles => {
                if accepts_images {
                    self.composer_attach_to_tab(tab_id, files);
                } else {
                    self.composer_warn_no_images();
                }
                true
            }
            PastePlan::PasteText => false,
            PastePlan::ReadBitmap => {
                let dir = attachment_dir(self.codex_home.as_deref());
                self.backend.spawn(async move {
                    let pasted = tokio::task::spawn_blocking(move || {
                        read_clipboard_bitmap(&dir, accepts_images)
                    })
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result);
                    crate::ui_thread::post(move |app| match pasted {
                        Ok(PastedBitmap::Saved(path)) => {
                            app.composer_attach_to_tab(tab_id, vec![path]);
                        }
                        Ok(PastedBitmap::NotAccepted) => app.composer_warn_no_images(),
                        Ok(PastedBitmap::Nothing) => {}
                        Err(err) => app.toast(format!("Could not paste the image: {err}")),
                    });
                });
                true
            }
        }
    }

    /// Attach button: pick image files.
    pub(super) fn composer_pick_images(&mut self) {
        let Some(index) = self.active_thread_index() else {
            return;
        };
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        if !super::thread_accepts_images(&self.composer_shared, thread) {
            self.composer_warn_no_images();
            return;
        }
        let tab_id = self.tabs[index].id;
        let mut dialog = rfd::AsyncFileDialog::new()
            .set_title("Attach images")
            .add_filter("Images", IMAGE_EXTENSIONS);
        // A remote thread's folder is not on this machine.
        if !self.backend.uses_remote_workspace() && thread.cwd.is_dir() {
            dialog = dialog.set_directory(&thread.cwd);
        }
        let future = dialog.pick_files();
        let spawned = slint::spawn_local(async move {
            let Some(files) = future.await else {
                return;
            };
            let paths: Vec<PathBuf> = files.iter().map(|file| file.path().to_path_buf()).collect();
            crate::ui_thread::with_app(move |app| app.composer_attach_to_tab(tab_id, paths));
        });
        if let Err(err) = spawned {
            tracing::warn!(%err, "could not open the file picker");
        }
    }

    /// Adds image files to the draft of tab `tab_id` and loads thumbnails.
    /// Files that cannot be decoded are dropped again with a message.
    pub(crate) fn composer_attach_to_tab(&mut self, tab_id: TabId, paths: Vec<PathBuf>) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let mut added = Vec::new();
        let mut skipped = 0usize;
        for path in paths {
            if !is_image_path(&path) {
                skipped += 1;
                continue;
            }
            if thread
                .composer
                .attachments
                .iter()
                .any(|existing| existing.path == path)
            {
                continue;
            }
            thread
                .composer
                .attachments
                .push(Attachment::new(path.clone()));
            added.push(path);
        }
        if skipped > 0 {
            self.toast("Only PNG, JPEG, GIF and WebP images can be attached");
        }
        for path in added {
            self.backend.spawn(async move {
                let source = path.clone();
                let loaded = tokio::task::spawn_blocking(move || load_thumbnail(&source))
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result);
                crate::ui_thread::post(move |app| app.composer_on_thumbnail(tab_id, &path, loaded));
            });
        }
        if self.active == Some(index) {
            self.composer_refresh();
            self.composer_focus();
        }
    }

    fn composer_on_thumbnail(
        &mut self,
        tab_id: TabId,
        path: &Path,
        loaded: anyhow::Result<(SharedPixelBuffer<Rgba8Pixel>, (u32, u32))>,
    ) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let Some(thread) = self.thread_tab_mut(index) else {
            return;
        };
        let Some(position) = thread
            .composer
            .attachments
            .iter()
            .position(|attachment| attachment.path == path)
        else {
            return;
        };
        match loaded {
            Ok((thumbnail, dimensions)) => {
                let attachment = &mut thread.composer.attachments[position];
                attachment.thumbnail = Some(thumbnail);
                attachment.dimensions = Some(dimensions);
            }
            Err(err) => {
                let removed = thread.composer.attachments.remove(position);
                self.toast(format!("Cannot attach {}: {err}", removed.file_name()));
            }
        }
        if self.active == Some(index) {
            self.composer_refresh();
        }
    }

    pub(super) fn composer_remove_attachment(&mut self, row: usize) {
        let Some(index) = self.active_thread_index() else {
            return;
        };
        if let Some(thread) = self.thread_tab_mut(index)
            && row < thread.composer.attachments.len()
        {
            thread.composer.attachments.remove(row);
        }
        self.composer_refresh();
    }

    fn composer_warn_no_images(&mut self) {
        let model = self
            .active_thread_index()
            .and_then(|index| self.thread_tab(index))
            .and_then(super::toolbar::effective_model);
        let label = super::presets::model_label(&self.composer_shared.models, model.as_deref());
        self.toast(format!("{label} does not accept images"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn recognises_image_extensions() {
        assert!(is_image_path(Path::new("a/b.PNG")));
        assert!(is_image_path(Path::new("shot.jpeg")));
        assert!(is_image_path(Path::new("anim.gif")));
        assert!(!is_image_path(Path::new("notes.txt")));
        assert!(!is_image_path(Path::new("png")));
    }

    #[test]
    fn paste_answers_from_cheap_checks() {
        assert_eq!(
            paste_plan(/*has_image_files*/ true, /*has_text*/ true),
            PastePlan::AttachFiles
        );
        assert_eq!(
            paste_plan(/*has_image_files*/ false, /*has_text*/ true),
            PastePlan::PasteText
        );
        // No files and no text: the bitmap (if any) is read in the
        // background instead of decoding it while the key is handled.
        assert_eq!(
            paste_plan(/*has_image_files*/ false, /*has_text*/ false),
            PastePlan::ReadBitmap
        );
    }

    #[test]
    fn saves_and_thumbnails_clipboard_pixels() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let rgba = vec![200u8; 120 * 80 * 4];
        let path = save_clipboard_png(dir.path(), 120, 80, rgba)?;
        assert!(path.starts_with(dir.path()));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("png"));
        let (thumbnail, dimensions) = load_thumbnail(&path)?;
        assert_eq!(dimensions, (120, 80));
        assert!(thumbnail.width() <= THUMBNAIL_SIZE && thumbnail.height() <= THUMBNAIL_SIZE);
        Ok(())
    }

    #[test]
    fn rejects_mismatched_buffers() {
        let dir = std::env::temp_dir();
        assert!(save_clipboard_png(&dir, 10, 10, vec![0u8; 12]).is_err());
    }

    #[test]
    fn attachment_dir_lives_under_codex_home() {
        assert_eq!(
            attachment_dir(Some(Path::new("/home/me/.codex"))),
            PathBuf::from("/home/me/.codex/tmp/gui-attachments")
        );
    }
}
