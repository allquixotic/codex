//! File tabs: chunked, binary-aware loading, live reload, find, go to line,
//! and pushing a tab's state into the `FilesState` global.
//!
//! Text tabs ([`AppController::open_text_tab`]) reuse all of it for text
//! that is not backed by a file: the text is kept in memory, decoded in the
//! same chunks, and never watched or reloaded.
//!
//! Content goes through the app-server where the protocol allows it:
//! `fs/getMetadata` for type and modification time, `fs/readFile` for file
//! content, and `fs/watch` for live reload. The protocol has no ranged
//! reads, so a large file that is also on this machine (same modification
//! time as the server reports, which holds for the embedded server and a
//! local daemon) is read locally on a blocking thread, one prefix at a time;
//! "Load more" re-reads a longer prefix. A remote server's files always come
//! through `fs/readFile`. Since `fs/changed` can be dropped under load,
//! showing a tab also compares the modification time.

use std::io::Read;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use base64::Engine;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::FsGetMetadataParams;
use codex_app_server_protocol::FsGetMetadataResponse;
use codex_app_server_protocol::FsReadFileParams;
use codex_app_server_protocol::FsReadFileResponse;
use codex_app_server_protocol::FsUnwatchParams;
use codex_app_server_protocol::FsUnwatchResponse;
use codex_app_server_protocol::FsWatchParams;
use codex_app_server_protocol::FsWatchResponse;
use codex_protocol::num_format::format_with_separators;
use codex_utils_absolute_path::AbsolutePathBuf;
use slint::ComponentHandle;
use slint::ModelRc;
use slint::SharedString;
use slint::VecModel;

use super::FileTabKind;
use super::external;
use super::find;
use super::tab_key;
use super::text;
use super::text_doc_at;
use super::text_doc_ref;
use crate::app::AppController;
use crate::app::DialogRequest;
use crate::app::TabId;
use crate::app::TabKind;
use crate::backend::Backend;
use crate::backend::BackendError;
use crate::ui::FilesState;
use crate::ui::FindMark;

/// "Go to line" loads more of a file automatically up to this size.
const MAX_AUTO_LOAD_BYTES: u64 = 32 * 1024 * 1024;
/// Lines above and below the visible ones that get match highlights, so
/// small scrolls do not rebuild them.
const MARK_MARGIN_LINES: usize = 60;
/// Highlights shown at once; matches beyond are found but not marked.
const MAX_MARKS: usize = 400;
/// Texts larger than this are searched once typing pauses for
/// [`FIND_DEBOUNCE`]; smaller ones on every keystroke.
const FIND_DEBOUNCE_BYTES: usize = 512 * 1024;
const FIND_DEBOUNCE: Duration = Duration::from_millis(150);

/// Why a file is being (re)read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LoadKind {
    /// First load of a new tab.
    Initial,
    /// Re-read what is shown (file changed, Reload, Retry).
    Reload,
    /// Show one more chunk.
    More,
    /// Decode a file that looked binary anyway.
    ShowAsText,
}

/// Text shown in a tab without a file behind it.
pub(super) struct MemoryText {
    pub(super) title: String,
    /// The whole text; `TextDoc.text` holds the decoded prefix shown.
    pub(super) text: Arc<str>,
}

/// State of one file tab.
pub(super) struct TextDoc {
    /// Absolute, lexically normalized path as opened (empty for text tabs).
    pub(super) path: PathBuf,
    /// Set for text tabs: the text comes from memory, not from `path`.
    pub(super) memory: Option<MemoryText>,
    /// Path with symlinks resolved, once loaded.
    canonical: Option<PathBuf>,
    text: SharedString,
    line_starts: Vec<usize>,
    numbered_lines: usize,
    /// Line numbers for the gutter, one per line.
    gutter: SharedString,
    /// Bumped whenever `text` changes.
    revision: u64,
    /// Revision currently in the global while this tab is shown.
    pushed_revision: Option<u64>,
    /// Length of the file prefix decoded into `text`.
    loaded_bytes: u64,
    total_bytes: u64,
    modified_ms: Option<i64>,
    complete: bool,
    binary: bool,
    show_binary: bool,
    loaded_once: bool,
    loading: bool,
    load_generation: u64,
    /// The read in flight; aborted when a newer load supersedes it.
    load_task: Option<tokio::task::JoinHandle<()>>,
    /// The file can run as a program (known for files on this machine).
    executable: bool,
    /// Shown instead of the text when nothing could be loaded.
    error: Option<String>,
    /// Banner above the text (the file vanished, a reload failed).
    notice: Option<String>,
    watch_id: Option<String>,
    /// Changed on disk while in the background; reloaded when shown.
    stale: bool,
    wrap: bool,
    scroll: (f32, f32),
    /// One-based line to reveal once loaded and shown.
    pending_line: Option<usize>,
    /// `loaded_bytes` when "go to line" last loaded more of the file to
    /// reach `pending_line`; the chain stops when a load adds nothing
    /// (it failed, or the file did not grow).
    auto_load_from: Option<u64>,
    cursor_label: String,
    find: FindState,
    /// Zero-based first and last visible line, as last reported by the view.
    view_lines: (usize, usize),
}

#[derive(Default)]
struct FindState {
    open: bool,
    query: String,
    matches: Vec<Range<usize>>,
    truncated: bool,
    current: Option<usize>,
    generation: u64,
    /// The latest generation, shared with the search running off the UI
    /// thread so a superseded search stops early.
    latest: Arc<AtomicU64>,
    /// Folded text of `TextDoc::revision`, reused while a query is typed.
    folded: Option<(u64, Arc<find::Folded>)>,
    /// Waits for typing to pause before searching a large text.
    debounce: slint::Timer,
    /// Byte offset the next search starts from (the cursor or current match).
    anchor: usize,
    /// Lines covered by the highlights in the global, while any are shown.
    marked_lines: Option<Range<usize>>,
}

impl TextDoc {
    pub(super) fn new(path: PathBuf, line: Option<usize>) -> Self {
        Self {
            path,
            canonical: None,
            text: SharedString::new(),
            line_starts: vec![0],
            numbered_lines: 0,
            gutter: SharedString::new(),
            revision: 0,
            pushed_revision: None,
            loaded_bytes: 0,
            total_bytes: 0,
            modified_ms: None,
            complete: false,
            binary: false,
            show_binary: false,
            loaded_once: false,
            loading: false,
            load_generation: 0,
            load_task: None,
            executable: false,
            error: None,
            notice: None,
            watch_id: None,
            stale: false,
            wrap: false,
            scroll: (0.0, 0.0),
            pending_line: line,
            auto_load_from: None,
            cursor_label: String::new(),
            find: FindState::default(),
            view_lines: (0, 0),
            memory: None,
        }
    }

    /// A text tab showing `text` under `title`.
    pub(super) fn in_memory(title: String, text: String) -> Self {
        let mut doc = Self::new(PathBuf::new(), /*line*/ None);
        doc.memory = Some(MemoryText {
            title,
            text: Arc::from(text),
        });
        doc
    }

    /// Replaces the text of a text tab and starts over at the top.
    pub(super) fn replace_memory(&mut self, text: String) {
        if let Some(memory) = self.memory.as_mut() {
            memory.text = Arc::from(text);
            self.loaded_bytes = 0;
            self.complete = false;
            self.scroll = (0.0, 0.0);
            self.pushed_revision = None;
        }
    }

    pub(super) fn is_memory(&self) -> bool {
        self.memory.is_some()
    }

    /// Tab and header title: the file name, or "<title> (text)".
    pub(super) fn title(&self) -> String {
        match &self.memory {
            Some(memory) => format!("{} (text)", memory.title),
            None => crate::app::folder_label(&self.path),
        }
    }

    /// Whether this tab shows `path` (as opened or after resolving links).
    pub(super) fn shows_path(&self, path: &Path) -> bool {
        !self.is_memory() && (self.path == path || self.canonical.as_deref() == Some(path))
    }

    /// Stores the view state the global held for this tab.
    pub(super) fn save_view(&mut self, scroll: (f32, f32), query: String) {
        self.scroll = scroll;
        self.find.query = query;
        self.pushed_revision = None;
    }

    fn extent_for(&self, kind: LoadKind) -> u64 {
        let extent = match kind {
            LoadKind::Initial => text::CHUNK_BYTES,
            LoadKind::Reload | LoadKind::ShowAsText => self.loaded_bytes.max(text::CHUNK_BYTES),
            LoadKind::More => self.loaded_bytes + text::CHUNK_BYTES,
        };
        extent.min(text::MAX_LOADED_BYTES)
    }

    fn shows_binary_notice(&self) -> bool {
        self.binary && !self.show_binary
    }

    /// Whether the text view (rather than a notice) is showing.
    fn shows_text(&self) -> bool {
        self.loaded_once && self.error.is_none() && !self.shows_binary_notice()
    }

    fn apply(&mut self, read: DocRead) {
        self.canonical = Some(read.canonical);
        self.total_bytes = read.total_bytes;
        self.modified_ms = Some(read.modified_ms);
        self.binary = read.binary;
        self.executable = read.executable;
        self.loaded_once = true;
        self.error = None;
        self.notice = None;
        let decoded = read.decoded.unwrap_or_else(Decoded::empty);
        if decoded.text != self.text.as_str() {
            self.text = decoded.text.into();
            self.line_starts = decoded.line_starts;
            self.numbered_lines = decoded.numbered_lines;
            self.gutter = decoded.gutter.into();
            self.revision += 1;
        }
        self.loaded_bytes = decoded.consumed;
        self.complete = decoded.complete;
    }

    fn fail(&mut self, error: LoadError) {
        let had_content = self.loaded_once;
        match error {
            LoadError::Missing if had_content => {
                self.notice = Some(
                    "This file was deleted or moved. Showing the last loaded version.".to_string(),
                );
            }
            LoadError::Missing => {
                self.error = Some(format!("{} does not exist.", self.path.display()));
            }
            LoadError::Failed(message) if had_content => {
                self.notice = Some(format!("Could not reload the file: {message}"));
            }
            LoadError::Failed(message) => self.error = Some(message),
        }
    }

    fn details(&self) -> String {
        if !self.loaded_once {
            return String::new();
        }
        let size = text::format_size(self.total_bytes);
        if self.shows_binary_notice() {
            return format!("{size} · binary");
        }
        let lines = format_with_separators(i64::try_from(self.numbered_lines).unwrap_or(i64::MAX));
        let noun = if self.numbered_lines == 1 {
            "line"
        } else {
            "lines"
        };
        if self.complete {
            format!("{size} · {lines} {noun}")
        } else {
            format!("{size} · {lines} {noun} loaded")
        }
    }

    /// `(truncated, message, can_load_more)` for the "Load more" bar.
    fn truncation(&self) -> (bool, String, bool) {
        if !self.loaded_once || self.complete || self.shows_binary_notice() || self.error.is_some()
        {
            return (false, String::new(), false);
        }
        let shown = text::format_size(self.loaded_bytes);
        let total = text::format_size(self.total_bytes);
        if self.loaded_bytes >= text::MAX_LOADED_BYTES {
            let rest = if self.is_memory() {
                "Use Copy all or Save as… for the rest."
            } else {
                "Open the file externally to see the rest."
            };
            (
                true,
                format!("Showing the first {shown} of {total}. {rest}"),
                false,
            )
        } else {
            (true, format!("Showing the first {shown} of {total}."), true)
        }
    }

    /// The file changed on disk; returns whether to reload now. Otherwise
    /// it is reloaded when shown, or when the load in flight finishes: a
    /// reload now would supersede that load (say "Load more", which takes
    /// a while on a big file) with one of the old extent, and on a file that
    /// keeps changing the longer read would never complete.
    fn note_change(&mut self, shown: bool) -> bool {
        if shown && !self.loading {
            return true;
        }
        self.stale = true;
        false
    }

    /// What a pending "go to line" needs next. Asks for more of the file
    /// while the line is not loaded yet, until a load adds nothing.
    fn pending_line_step(&mut self) -> PendingLineStep {
        let Some(line) = self.pending_line else {
            return PendingLineStep::Nothing;
        };
        if self.loading || !self.loaded_once || self.error.is_some() || self.shows_binary_notice() {
            return PendingLineStep::Wait;
        }
        let reachable = line <= self.numbered_lines;
        // The load asked for last time failed or found no more text: stop
        // instead of asking again forever.
        let stalled = self
            .auto_load_from
            .take()
            .is_some_and(|from| self.loaded_bytes <= from);
        if !reachable
            && !stalled
            && !self.complete
            && self.loaded_bytes < MAX_AUTO_LOAD_BYTES.min(text::MAX_LOADED_BYTES)
        {
            self.auto_load_from = Some(self.loaded_bytes);
            return PendingLineStep::LoadMore;
        }
        PendingLineStep::Reveal
    }

    /// Find bar status text and whether it reports "no results".
    fn find_status(&self) -> (String, bool) {
        let find = &self.find;
        if find.query.is_empty() {
            return (String::new(), false);
        }
        if find.matches.is_empty() {
            return ("No results".to_string(), true);
        }
        let more = if find.truncated { "+" } else { "" };
        let total = format!(
            "{}{more}",
            format_with_separators(i64::try_from(find.matches.len()).unwrap_or(i64::MAX))
        );
        match find.current {
            Some(current) => (format!("{} of {total}", current + 1), false),
            None => (format!("{total} matches"), false),
        }
    }
}

/// See [`TextDoc::pending_line_step`].
#[derive(Debug, Eq, PartialEq)]
enum PendingLineStep {
    Nothing,
    /// Loading, or nothing to show the line in.
    Wait,
    LoadMore,
    /// Show the line, or the nearest one when it cannot be reached.
    Reveal,
}

/// What one load read from disk.
pub(super) struct DocRead {
    canonical: PathBuf,
    total_bytes: u64,
    modified_ms: i64,
    binary: bool,
    executable: bool,
    /// `None` for binary files that were not forced to text.
    decoded: Option<Decoded>,
}

/// Display text and its line index, built off the UI thread.
struct Decoded {
    text: String,
    line_starts: Vec<usize>,
    numbered_lines: usize,
    gutter: String,
    consumed: u64,
    complete: bool,
}

impl Decoded {
    fn new(text: String, consumed: u64, complete: bool) -> Self {
        let line_starts = text::line_starts(&text);
        let numbered_lines = text::numbered_lines(&text, &line_starts);
        Self {
            gutter: text::gutter_numbers(numbered_lines),
            text,
            line_starts,
            numbered_lines,
            consumed,
            complete,
        }
    }

    fn empty() -> Self {
        Self::new(String::new(), /*consumed*/ 0, /*complete*/ false)
    }
}

pub(super) enum LoadError {
    Missing,
    Failed(String),
}

/// How [`read_document`] reads.
#[derive(Clone, Copy, Debug)]
struct ReadOptions {
    /// Bytes from the start of the file to show.
    extent: u64,
    /// Decode a file that looks binary anyway.
    force_text: bool,
    /// The server runs in this process, so its files are local files.
    local_server: bool,
}

async fn get_metadata(
    backend: &Backend,
    path: &AbsolutePathBuf,
) -> Result<FsGetMetadataResponse, LoadError> {
    backend
        .request(ClientRequest::FsGetMetadata {
            request_id: backend.next_request_id(),
            params: FsGetMetadataParams { path: path.clone() },
        })
        .await
        .map_err(|err| classify_failure(&err))
}

async fn probe_local(path: &Path, server_modified_ms: i64, trusted: bool) -> LocalFile {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || local_twin(&path, server_modified_ms, trusted))
        .await
        .unwrap_or(LocalFile::Absent)
}

async fn read_document(
    backend: &Backend,
    path: PathBuf,
    options: ReadOptions,
) -> Result<DocRead, LoadError> {
    let ReadOptions {
        extent,
        force_text,
        local_server,
    } = options;
    let absolute = AbsolutePathBuf::try_from(path.clone())
        .map_err(|err| LoadError::Failed(format!("{}: {err}", path.display())))?;
    let metadata = get_metadata(backend, &absolute).await?;
    if metadata.is_directory {
        return Err(LoadError::Failed(format!(
            "{} is a folder.",
            path.display()
        )));
    }
    if !metadata.is_file {
        // Devices and pipes could block a read forever.
        return Err(LoadError::Failed(format!(
            "{} is not a regular file.",
            path.display()
        )));
    }
    let mut modified_ms = metadata.modified_at_ms;
    let mut local = probe_local(&path, modified_ms, local_server).await;
    if matches!(local, LocalFile::Changed) {
        // A write landed between the server's stat and ours (a log being
        // appended): look once more before reading the whole file through
        // the server.
        modified_ms = get_metadata(backend, &absolute).await?.modified_at_ms;
        local = probe_local(&path, modified_ms, local_server).await;
    }
    let executable = match &local {
        LocalFile::Same { executable, .. } => *executable,
        LocalFile::Absent | LocalFile::Changed => false,
    };
    let (bytes, total_bytes, canonical, eof) = match local {
        // Large files on this machine are read one prefix at a time.
        LocalFile::Same {
            len,
            canonical,
            modified_ms: local_modified,
            ..
        } if len > text::CHUNK_BYTES => {
            modified_ms = local_modified;
            let read_path = path.clone();
            let (bytes, eof) = blocking(move || read_prefix(&read_path, extent)).await?;
            (bytes, len, canonical, eof)
        }
        // Small files, and every file of a remote server, go through
        // `fs/readFile`, which returns the whole file.
        local => {
            let mut bytes = read_file_rpc(backend, absolute).await?;
            let total = bytes.len() as u64;
            let eof = total <= extent;
            bytes.truncate(usize::try_from(extent).unwrap_or(usize::MAX));
            let canonical = match local {
                LocalFile::Same { canonical, .. } => canonical,
                LocalFile::Absent | LocalFile::Changed => path,
            };
            (bytes, total, canonical, eof)
        }
    };
    let (binary, decoded) = blocking(move || Ok(decode_document(&bytes, eof, force_text))).await?;
    Ok(DocRead {
        canonical,
        total_bytes,
        modified_ms,
        binary,
        executable,
        decoded,
    })
}

/// Decodes the first `extent` bytes of a text tab's text (cut at a line end
/// or character boundary, like file chunks).
fn read_memory(text: &str, extent: u64) -> DocRead {
    let bytes = text.as_bytes();
    let total_bytes = bytes.len() as u64;
    let eof = total_bytes <= extent;
    let shown = &bytes[..usize::try_from(extent.min(total_bytes)).unwrap_or(bytes.len())];
    DocRead {
        canonical: PathBuf::new(),
        total_bytes,
        modified_ms: 0,
        binary: false,
        executable: false,
        decoded: decode_document(shown, eof, /*force_text*/ true).1,
    }
}

async fn read_file_rpc(backend: &Backend, path: AbsolutePathBuf) -> Result<Vec<u8>, LoadError> {
    let response: FsReadFileResponse = backend
        .request(ClientRequest::FsReadFile {
            request_id: backend.next_request_id(),
            params: FsReadFileParams { path },
        })
        .await
        .map_err(|err| classify_failure(&err))?;
    base64::engine::general_purpose::STANDARD
        .decode(response.data_base64)
        .map_err(|err| LoadError::Failed(format!("the server returned invalid file data: {err}")))
}

/// What this machine has at a path the server described.
#[derive(Debug, Eq, PartialEq)]
enum LocalFile {
    /// The file the server described.
    Same {
        len: u64,
        canonical: PathBuf,
        modified_ms: i64,
        executable: bool,
    },
    /// A file whose modification time differs: another machine's file of
    /// the same path, or the file changed since the server looked.
    Changed,
    /// No regular file (a remote server's path).
    Absent,
}

/// `path` on this machine when it is the file the server described: same
/// modification time, or `trusted` (the server runs in this process, so a
/// different time only means the file changed in between).
fn local_twin(path: &Path, server_modified_ms: i64, trusted: bool) -> LocalFile {
    let Ok(metadata) = std::fs::metadata(path) else {
        return LocalFile::Absent;
    };
    if !metadata.is_file() {
        return LocalFile::Absent;
    }
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|modified| i64::try_from(modified.as_millis()).ok());
    let modified_ms = match modified_ms {
        Some(modified_ms) if modified_ms == server_modified_ms || trusted => modified_ms,
        Some(_) => return LocalFile::Changed,
        None if trusted => server_modified_ms,
        None => return LocalFile::Absent,
    };
    let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    LocalFile::Same {
        len: metadata.len(),
        canonical,
        modified_ms,
        executable: external::is_executable(path, &metadata),
    }
}

fn classify_failure(err: &BackendError) -> LoadError {
    let message = err.user_message();
    if is_not_found_message(&message) {
        LoadError::Missing
    } else {
        LoadError::Failed(message)
    }
}

/// Whether a server error reports a missing file. The server forwards the
/// OS error text, which ends in `(os error 2)` for ENOENT and Windows
/// ERROR_FILE_NOT_FOUND, or `(os error 3)` for ERROR_PATH_NOT_FOUND.
fn is_not_found_message(message: &str) -> bool {
    message.contains("(os error 2)") || message.contains("(os error 3)")
}

async fn blocking<T, F>(f: F) -> Result<T, LoadError>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => Err(LoadError::Missing),
        Ok(Err(err)) => Err(LoadError::Failed(err.to_string())),
        Err(err) => Err(LoadError::Failed(err.to_string())),
    }
}

/// Reads up to `extent` bytes from the start of `path`; also reports
/// whether that reached the end of the file.
fn read_prefix(path: &Path, extent: u64) -> std::io::Result<(Vec<u8>, bool)> {
    let file = std::fs::File::open(path)?;
    let total = file.metadata()?.len();
    let want = extent.min(total);
    let mut bytes = Vec::with_capacity(usize::try_from(want).unwrap_or(0));
    file.take(want).read_to_end(&mut bytes)?;
    let eof = bytes.len() as u64 >= total;
    Ok((bytes, eof))
}

fn decode_document(bytes: &[u8], eof: bool, force_text: bool) -> (bool, Option<Decoded>) {
    let binary = text::looks_binary(bytes);
    if binary && !force_text {
        return (true, None);
    }
    let cut = text::chunk_cut(bytes, eof);
    let content = text::decode_text(&bytes[..cut], /*at_file_start*/ true);
    (
        binary,
        Some(Decoded::new(
            content,
            cut as u64,
            /*complete*/ eof && cut == bytes.len(),
        )),
    )
}

/// `path` with the home directory shortened to `~`.
fn display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(relative) = path.strip_prefix(&home)
    {
        return Path::new("~").join(relative).display().to_string();
    }
    path.display().to_string()
}

/// Runs the launcher for `path` (blocking) and reports a failure.
fn files_launch(action: external::ExternalAction, path: &Path) {
    if let Err(message) = external::run(action, path) {
        crate::ui_thread::post(move |app| app.toast(message));
    }
}

fn to_i32(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn digits(value: usize) -> usize {
    value.max(1).to_string().len()
}

impl AppController {
    /// Starts reading the file of tab `index`.
    pub(super) fn files_load(&mut self, index: usize, kind: LoadKind) {
        let Some(tab_id) = self.tabs.get(index).map(|tab| tab.id) else {
            return;
        };
        let Some(doc) = text_doc_at(&mut self.tabs, index) else {
            return;
        };
        if kind == LoadKind::ShowAsText {
            doc.show_binary = true;
        }
        let extent = doc.extent_for(kind);
        doc.load_generation += 1;
        doc.loading = true;
        doc.stale = false;
        // Whole-file reads of a superseded load must not pile up.
        if let Some(task) = doc.load_task.take() {
            task.abort();
        }
        let generation = doc.load_generation;
        if let Some(memory) = &doc.memory {
            let text = Arc::clone(&memory.text);
            self.backend.runtime().spawn_blocking(move || {
                let read = read_memory(&text, extent);
                crate::ui_thread::post(move |app| {
                    app.files_text_loaded(tab_id, generation, Ok(read));
                });
            });
        } else {
            let path = doc.path.clone();
            let options = ReadOptions {
                extent,
                force_text: doc.show_binary,
                // The embedded server runs in this process: its files are
                // this machine's files.
                local_server: self.config.is_some(),
            };
            let backend = self.backend.clone();
            doc.load_task = Some(self.backend.spawn(async move {
                let result = read_document(&backend, path, options).await;
                crate::ui_thread::post(move |app| {
                    app.files_text_loaded(tab_id, generation, result);
                });
            }));
        }
        if self.files_is_shown(index) {
            self.files_push_text(index, /*switching*/ false);
        }
    }

    fn files_text_loaded(
        &mut self,
        tab_id: TabId,
        generation: u64,
        result: Result<DocRead, LoadError>,
    ) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let first_load = match text_doc_ref(&self.tabs, index) {
            Some(doc) if doc.load_generation == generation => !doc.loaded_once && !doc.is_memory(),
            _ => return,
        };
        let duplicate = match &result {
            Ok(read) if first_load => self.files_other_tab_for(index, &read.canonical),
            _ => None,
        };
        if let Some(existing) = duplicate {
            // The same file under another path (a symlink): keep one tab.
            let line = text_doc_at(&mut self.tabs, index).and_then(|doc| doc.pending_line.take());
            let existing_id = self.tabs[existing].id;
            self.close_tab(index);
            if let Some(existing) = self.tab_index_by_id(existing_id) {
                self.activate_tab(existing);
                if let Some(line) = line {
                    self.files_go_to_line(existing, line);
                }
            }
            return;
        }
        let Some(doc) = text_doc_at(&mut self.tabs, index) else {
            return;
        };
        doc.loading = false;
        doc.load_task = None;
        let rerun_find = match result {
            Ok(read) => {
                let revision = doc.revision;
                doc.apply(read);
                doc.revision != revision && doc.find.open && !doc.find.query.is_empty()
            }
            Err(error) => {
                doc.fail(error);
                false
            }
        };
        self.files_watch(index);
        if rerun_find {
            self.files_run_find(index, /*select*/ false);
        }
        if self.files_is_shown(index) {
            self.files_push_text(index, /*switching*/ false);
        }
        self.files_apply_pending_line(index);
        // The file changed while this load ran (see `files_on_fs_changed`);
        // a load started above reads it again anyway.
        let reload = self.files_is_shown(index)
            && text_doc_ref(&self.tabs, index).is_some_and(|doc| doc.stale && !doc.loading);
        if reload {
            self.files_load(index, LoadKind::Reload);
        }
    }

    /// Another file tab already showing `canonical`.
    fn files_other_tab_for(&self, index: usize, canonical: &Path) -> Option<usize> {
        self.tabs
            .iter()
            .enumerate()
            .find_map(|(other, tab)| match &tab.kind {
                TabKind::File(file) if other != index => match &file.kind {
                    FileTabKind::Text(doc) if doc.shows_path(canonical) => Some(other),
                    _ => None,
                },
                _ => None,
            })
    }

    /// Pushes tab `index` into the `FilesState` global. `switching` means
    /// the global held another tab, so everything is replaced.
    pub(super) fn files_push_text(&mut self, index: usize, switching: bool) {
        let Some(key) = self.tabs.get(index).map(|tab| tab_key(tab.id)) else {
            return;
        };
        let state = self.window.global::<FilesState>();
        let Some(doc) = text_doc_at(&mut self.tabs, index) else {
            return;
        };
        state.set_tab_id(key);
        state.set_is_diff(false);
        state.set_is_text(doc.is_memory());
        state.set_title(doc.title().into());
        state.set_path(if doc.is_memory() {
            "Read-only text, not saved to a file".into()
        } else {
            display_path(&doc.path).into()
        });
        state.set_details(doc.details().into());
        state.set_loading(doc.loading);
        state.set_error(doc.error.clone().unwrap_or_default().into());
        state.set_notice(doc.notice.clone().unwrap_or_default().into());
        state.set_binary(doc.shows_binary_notice());
        state.set_executable(doc.executable);
        let size = text::format_size(doc.total_bytes);
        state.set_binary_info(
            if doc.executable {
                format!(
                    "This file ({size}) is a program: opening it would run it. Reveal it in the file manager, or show it as text."
                )
            } else {
                format!(
                    "This file ({size}) does not look like text. Open it with another app, or show it as text anyway."
                )
            }
            .into(),
        );
        if switching || doc.pushed_revision != Some(doc.revision) {
            state.set_content(doc.text.clone());
            state.set_visual_lines(to_i32(doc.line_starts.len()));
            state.set_line_count(to_i32(doc.numbered_lines));
            state.set_gutter_text(doc.gutter.clone());
            state.set_gutter_digits(to_i32(digits(doc.numbered_lines).max(3)));
            doc.pushed_revision = Some(doc.revision);
        }
        let (truncated, truncated_info, can_load_more) = doc.truncation();
        state.set_truncated(truncated);
        state.set_truncated_info(truncated_info.into());
        state.set_can_load_more(can_load_more && !doc.loading);
        state.set_wrap(doc.wrap);
        state.set_cursor_label(if doc.shows_text() {
            doc.cursor_label.as_str().into()
        } else {
            SharedString::new()
        });
        state.set_find_open(doc.find.open);
        let (status, no_match) = doc.find_status();
        state.set_find_status(status.into());
        state.set_find_no_match(no_match);
        if switching {
            doc.find.marked_lines = None;
            state.set_find_query(doc.find.query.as_str().into());
            state.set_scroll_x(doc.scroll.0);
            state.set_scroll_y(doc.scroll.1);
            state.set_restore_x(doc.scroll.0);
            state.set_restore_y(doc.scroll.1);
            state.set_restore_pending(true);
            state.set_sel_fresh(false);
        }
        self.files_push_find_marks(index, /*force*/ switching);
    }

    /// Reloads tab `index` if its file changed since it was read.
    pub(super) fn files_check_fresh(&mut self, index: usize) {
        let Some(tab_id) = self.tabs.get(index).map(|tab| tab.id) else {
            return;
        };
        let Some(doc) = text_doc_at(&mut self.tabs, index) else {
            return;
        };
        if doc.loading || !doc.loaded_once || doc.is_memory() {
            return;
        }
        if doc.stale {
            self.files_load(index, LoadKind::Reload);
            return;
        }
        let known = doc.modified_ms;
        let Ok(path) = AbsolutePathBuf::try_from(doc.path.clone()) else {
            return;
        };
        self.files_watch(index);
        self.backend.call(
            |request_id| ClientRequest::FsGetMetadata {
                request_id,
                params: FsGetMetadataParams { path },
            },
            move |app, result: Result<FsGetMetadataResponse, BackendError>| {
                let changed = match result {
                    Ok(metadata) => Some(metadata.modified_at_ms) != known,
                    // Let the reload report what went wrong.
                    Err(_) => true,
                };
                if changed
                    && let Some(index) = app.tab_index_by_id(tab_id)
                    && text_doc_at(&mut app.tabs, index).is_some_and(|doc| !doc.loading)
                {
                    app.files_load(index, LoadKind::Reload);
                }
            },
        );
    }

    /// Registers an `fs/watch` for tab `index` unless it has one.
    fn files_watch(&mut self, index: usize) {
        let Some(tab_id) = self.tabs.get(index).map(|tab| tab.id) else {
            return;
        };
        let Some(doc) = text_doc_at(&mut self.tabs, index) else {
            return;
        };
        if doc.watch_id.is_some() || !doc.loaded_once || doc.is_memory() {
            return;
        }
        let target = doc.canonical.clone().unwrap_or_else(|| doc.path.clone());
        let Ok(path) = AbsolutePathBuf::try_from(target) else {
            return;
        };
        let watch_id = format!("codex-gui-file-{}", uuid::Uuid::new_v4());
        doc.watch_id = Some(watch_id.clone());
        let request_watch_id = watch_id.clone();
        self.backend.call(
            move |request_id| ClientRequest::FsWatch {
                request_id,
                params: FsWatchParams {
                    watch_id: request_watch_id,
                    path,
                },
            },
            move |app, result: Result<FsWatchResponse, BackendError>| {
                let index = app.tab_index_by_id(tab_id).filter(|&index| {
                    text_doc_ref(&app.tabs, index)
                        .is_some_and(|doc| doc.watch_id.as_deref() == Some(watch_id.as_str()))
                });
                match (result, index) {
                    (Ok(_), Some(_)) => {}
                    // The tab closed (or re-watched) before the watch existed.
                    (Ok(_), None) => app.files_send_unwatch(watch_id),
                    (Err(err), index) => {
                        tracing::warn!(error = %err, "fs/watch failed; changes are detected when the tab is shown");
                        if let Some(doc) = index.and_then(|index| text_doc_at(&mut app.tabs, index)) {
                            doc.watch_id = None;
                        }
                    }
                }
            },
        );
    }

    /// Releases what a closed tab holds: its watch, and a read or search in
    /// flight.
    pub(super) fn files_unwatch(&mut self, doc: TextDoc) {
        if let Some(task) = doc.load_task {
            task.abort();
        }
        doc.find.latest.store(u64::MAX, Ordering::Relaxed);
        if let Some(watch_id) = doc.watch_id {
            self.files_send_unwatch(watch_id);
        }
    }

    fn files_send_unwatch(&self, watch_id: String) {
        self.backend
            .fire::<FsUnwatchResponse, _>(|request_id| ClientRequest::FsUnwatch {
                request_id,
                params: FsUnwatchParams { watch_id },
            });
    }

    pub(super) fn files_rewatch_all(&mut self) {
        for index in 0..self.tabs.len() {
            if let Some(doc) = text_doc_at(&mut self.tabs, index) {
                // The old server's watches died with its connection.
                doc.watch_id = None;
                self.files_watch(index);
            }
        }
        if let Some(index) = self.files_active_index() {
            self.files_check_fresh(index);
        }
    }

    pub(super) fn files_on_fs_changed(&mut self, watch_id: &str) {
        let Some(index) = (0..self.tabs.len()).find(|&index| {
            text_doc_ref(&self.tabs, index)
                .is_some_and(|doc| doc.watch_id.as_deref() == Some(watch_id))
        }) else {
            return;
        };
        let shown = self.files_is_shown(index);
        if text_doc_at(&mut self.tabs, index).is_some_and(|doc| doc.note_change(shown)) {
            self.files_load(index, LoadKind::Reload);
        }
    }

    pub(super) fn files_reload_active(&mut self) {
        if let Some(index) = self.files_active_index()
            && text_doc_at(&mut self.tabs, index).is_some_and(|doc| !doc.is_memory())
        {
            self.files_load(index, LoadKind::Reload);
        }
    }

    pub(super) fn files_load_more(&mut self) {
        if let Some(index) = self.files_active_index()
            && text_doc_at(&mut self.tabs, index)
                .is_some_and(|doc| !doc.loading && doc.truncation().2)
        {
            self.files_load(index, LoadKind::More);
        }
    }

    pub(super) fn files_show_as_text(&mut self) {
        if let Some(index) = self.files_active_index()
            && text_doc_at(&mut self.tabs, index).is_some()
        {
            self.files_load(index, LoadKind::ShowAsText);
        }
    }

    pub(super) fn files_toggle_wrap(&mut self) {
        let Some(index) = self.files_active_index() else {
            return;
        };
        let state = self.window.global::<FilesState>();
        if let Some(doc) = text_doc_at(&mut self.tabs, index) {
            doc.wrap = !doc.wrap;
            state.set_wrap(doc.wrap);
        }
        self.files_push_find_marks(index, /*force*/ true);
    }

    /// Copies the whole text of a text tab (also the part not loaded yet),
    /// or the loaded text of a file.
    pub(super) fn files_copy_all(&mut self) {
        let text = self
            .files_active_index()
            .and_then(|index| text_doc_ref(&self.tabs, index))
            .map(|doc| match &doc.memory {
                Some(memory) => memory.text.to_string(),
                None => doc.text.to_string(),
            });
        if let Some(text) = text {
            self.copy_to_clipboard(&text);
        }
    }

    /// Asks where to save a text tab's text and writes it there.
    pub(super) fn files_save_text_as(&mut self) {
        let Some((title, content)) = self
            .files_active_index()
            .and_then(|index| text_doc_ref(&self.tabs, index))
            .and_then(|doc| doc.memory.as_ref())
            .map(|memory| (memory.title.clone(), Arc::clone(&memory.text)))
        else {
            return;
        };
        let mut dialog = rfd::AsyncFileDialog::new()
            .set_title("Save text as")
            .set_file_name(format!("{}.md", text::file_stem_for(&title)))
            .add_filter("Markdown", &["md"])
            .add_filter("Text", &["txt"]);
        if let Some(folder) = self
            .active_thread_index()
            .and_then(|index| self.thread_tab(index))
            .map(|thread| thread.cwd.clone())
            .filter(|cwd| cwd.is_absolute())
        {
            dialog = dialog.set_directory(folder);
        }
        let future = dialog.save_file();
        let runtime = self.backend.runtime().clone();
        let spawned = slint::spawn_local(async move {
            let Some(handle) = future.await else {
                return;
            };
            let path = handle.path().to_path_buf();
            runtime.spawn_blocking(move || {
                let message = match std::fs::write(&path, content.as_bytes()) {
                    Ok(()) => format!("Saved {}", path.display()),
                    Err(err) => format!("Could not save {}: {err}", path.display()),
                };
                crate::ui_thread::post(move |app| app.toast(message));
            });
        });
        if let Err(err) = spawned {
            tracing::warn!(%err, "could not open the save dialog");
        }
    }

    pub(super) fn files_copy_path(&mut self) {
        let path = self
            .files_active_index()
            .and_then(|index| text_doc_at(&mut self.tabs, index))
            .filter(|doc| !doc.is_memory())
            .map(|doc| doc.path.display().to_string());
        if let Some(path) = path {
            self.copy_to_clipboard(&path);
        }
    }

    pub(super) fn files_open_external(&mut self, action: external::ExternalAction) {
        let Some(path) = self
            .files_active_index()
            .and_then(|index| text_doc_at(&mut self.tabs, index))
            .filter(|doc| !doc.is_memory())
            .map(|doc| doc.path.clone())
        else {
            return;
        };
        // File tabs show the server's files; a launcher here would act on
        // whatever this machine has at the same path.
        if self.server_is_remote() {
            self.toast(format!("{} is on the server's machine", path.display()));
            return;
        }
        self.backend.runtime().spawn_blocking(move || {
            if action == external::ExternalAction::Open && external::is_executable_now(&path) {
                crate::ui_thread::post(move |app| app.files_confirm_open_program(path));
                return;
            }
            files_launch(action, &path);
        });
    }

    /// Asks before "opening" a file that would run as a program.
    fn files_confirm_open_program(&mut self, path: PathBuf) {
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let request = DialogRequest::confirm(
            "Run this file?",
            format!(
                "{name} is a program or script. Opening it runs it on this computer, outside the agent's sandbox. Use Reveal to see it in the file manager instead."
            ),
        )
        .accept_label("Run")
        .destructive();
        let runtime = self.backend.runtime().clone();
        self.show_dialog(
            request,
            Box::new(move |_, accepted| {
                if accepted.is_some() {
                    runtime.spawn_blocking(move || {
                        files_launch(external::ExternalAction::Open, &path);
                    });
                }
            }),
        );
    }

    pub(super) fn files_cursor_moved(&mut self, offset: i32) {
        let Some(index) = self
            .files_active_index()
            .filter(|&index| self.files_is_shown(index))
        else {
            return;
        };
        let state = self.window.global::<FilesState>();
        let Some(doc) = text_doc_at(&mut self.tabs, index).filter(|doc| doc.shows_text()) else {
            return;
        };
        let offset = usize::try_from(offset).unwrap_or(0);
        let (line, column) = text::line_col(&doc.text, &doc.line_starts, offset);
        doc.cursor_label = format!("Ln {line}, Col {column}");
        doc.find.anchor = offset;
        state.set_cursor_label(doc.cursor_label.as_str().into());
    }

    // ----- go to line ----------------------------------------------------

    pub(super) fn files_prompt_go_to_line(&mut self) {
        let Some(index) = self.files_active_index() else {
            return;
        };
        let tab_id = self.tabs[index].id;
        let Some(doc) = text_doc_at(&mut self.tabs, index).filter(|doc| doc.shows_text()) else {
            return;
        };
        let placeholder = if doc.complete && doc.numbered_lines > 0 {
            format!("Line number (1–{})", doc.numbered_lines)
        } else {
            "Line number".to_string()
        };
        let mut request = DialogRequest::prompt("Go to line", "").accept_label("Go");
        request.input_placeholder = placeholder;
        self.show_dialog(
            request,
            Box::new(move |app, input| {
                let Some(input) = input else {
                    return;
                };
                let Some(index) = app.tab_index_by_id(tab_id) else {
                    return;
                };
                match input.trim().parse::<usize>() {
                    Ok(line) if line > 0 => app.files_go_to_line(index, line),
                    _ => app.toast("Enter a line number"),
                }
            }),
        );
    }

    /// Reveals one-based `line` of tab `index`, loading more of the file
    /// first when needed.
    pub(super) fn files_go_to_line(&mut self, index: usize, line: usize) {
        if let Some(doc) = text_doc_at(&mut self.tabs, index) {
            doc.pending_line = Some(line.max(1));
            doc.auto_load_from = None;
            self.files_apply_pending_line(index);
        }
    }

    /// Applies the pending "go to line" once its line is loaded and the tab
    /// is shown.
    pub(super) fn files_apply_pending_line(&mut self, index: usize) {
        let shown = self.files_is_shown(index);
        let Some(doc) = text_doc_at(&mut self.tabs, index) else {
            return;
        };
        match doc.pending_line_step() {
            PendingLineStep::Nothing | PendingLineStep::Wait => return,
            PendingLineStep::LoadMore => {
                self.files_load(index, LoadKind::More);
                return;
            }
            PendingLineStep::Reveal => {}
        }
        // Kept until the tab is shown (`files_show`).
        if !shown {
            return;
        }
        let Some(line) = doc.pending_line.take() else {
            return;
        };
        let reachable = line <= doc.numbered_lines;
        let total = doc.numbered_lines;
        let complete = doc.complete;
        let range = text::line_range(&doc.text, &doc.line_starts, line.min(total.max(1)));
        if let Some(range) = range {
            self.files_push_selection(range.end, range.start);
        }
        if !reachable {
            self.toast(if complete {
                format!("The file has {total} lines")
            } else {
                format!("Line {line} is beyond the loaded part of the file")
            });
        }
    }

    /// Selects `anchor..focus` (UTF-8 byte offsets) in the shown text and
    /// scrolls it into the middle of the view.
    fn files_push_selection(&self, anchor: usize, focus: usize) {
        let state = self.window.global::<FilesState>();
        state.set_sel_anchor(to_i32(anchor));
        state.set_sel_focus(to_i32(focus));
        state.set_sel_fresh(true);
        state.set_sel_serial(state.get_sel_serial().wrapping_add(1));
    }

    // ----- find ------------------------------------------------------------

    pub(super) fn files_open_find(&mut self) {
        let Some(index) = self.files_active_index() else {
            return;
        };
        let state = self.window.global::<FilesState>();
        let Some(doc) = text_doc_at(&mut self.tabs, index).filter(|doc| doc.shows_text()) else {
            return;
        };
        let rerun = !doc.find.open && !doc.find.query.is_empty();
        doc.find.open = true;
        state.set_find_open(true);
        state.set_find_focus_serial(state.get_find_focus_serial().wrapping_add(1));
        if rerun {
            self.files_run_find(index, /*select*/ true);
        }
    }

    pub(super) fn files_close_find(&mut self) {
        let Some(index) = self.files_active_index() else {
            return;
        };
        let state = self.window.global::<FilesState>();
        if let Some(doc) = text_doc_at(&mut self.tabs, index) {
            doc.find.open = false;
            // The folded copy is as large as the text; keep it only while
            // searching.
            doc.find.folded = None;
            doc.find.debounce.stop();
            state.set_find_open(false);
        }
        self.files_push_find_marks(index, /*force*/ true);
    }

    pub(super) fn files_find_edited(&mut self, query: String) {
        let Some(index) = self.files_active_index() else {
            return;
        };
        let tab_id = self.tabs[index].id;
        if let Some(doc) = text_doc_at(&mut self.tabs, index) {
            if let Some(current) = doc
                .find
                .current
                .and_then(|current| doc.find.matches.get(current))
            {
                // Keep extending the current match while typing.
                doc.find.anchor = current.start;
            }
            doc.find.query = query;
            if doc.text.len() > FIND_DEBOUNCE_BYTES && !doc.find.query.is_empty() {
                // Searching a large text on every keystroke would queue a
                // full pass per character.
                doc.find
                    .debounce
                    .start(slint::TimerMode::SingleShot, FIND_DEBOUNCE, move || {
                        crate::ui_thread::with_app(move |app| {
                            if let Some(index) = app.tab_index_by_id(tab_id) {
                                app.files_run_find(index, /*select*/ true);
                            }
                        });
                    });
                return;
            }
            self.files_run_find(index, /*select*/ true);
        }
    }

    /// Searches tab `index` for its find query off the UI thread. A search
    /// still running for an older query or text stops early.
    fn files_run_find(&mut self, index: usize, select: bool) {
        let Some(tab_id) = self.tabs.get(index).map(|tab| tab.id) else {
            return;
        };
        let Some(doc) = text_doc_at(&mut self.tabs, index) else {
            return;
        };
        doc.find.debounce.stop();
        doc.find.generation += 1;
        let generation = doc.find.generation;
        doc.find.latest.store(generation, Ordering::Relaxed);
        if doc.find.query.is_empty() {
            doc.find.matches.clear();
            doc.find.truncated = false;
            doc.find.current = None;
            self.files_push_find_status(index);
            self.files_push_find_marks(index, /*force*/ true);
            return;
        }
        let revision = doc.revision;
        let folded = doc
            .find
            .folded
            .as_ref()
            .filter(|(folded_revision, _)| *folded_revision == revision)
            .map(|(_, folded)| Arc::clone(folded));
        let haystack = doc.text.clone();
        let needle = doc.find.query.clone();
        let latest = Arc::clone(&doc.find.latest);
        self.backend.runtime().spawn_blocking(move || {
            let cancelled = || latest.load(Ordering::Relaxed) != generation;
            let folded = match folded {
                Some(folded) => folded,
                None => match find::fold_cancellable(&haystack, &cancelled) {
                    Some(folded) => Arc::new(folded),
                    None => return,
                },
            };
            let Some(result) = find::find_in(&folded, &needle, find::MAX_MATCHES, &cancelled)
            else {
                return;
            };
            crate::ui_thread::post(move |app| {
                app.files_find_done(tab_id, generation, revision, folded, result, select);
            });
        });
    }

    fn files_find_done(
        &mut self,
        tab_id: TabId,
        generation: u64,
        revision: u64,
        folded: Arc<find::Folded>,
        result: find::FindResult,
        select: bool,
    ) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let Some(doc) = text_doc_at(&mut self.tabs, index) else {
            return;
        };
        if doc.revision == revision {
            doc.find.folded = Some((revision, folded));
        }
        if doc.find.generation != generation {
            return;
        }
        doc.find.matches = result.matches;
        doc.find.truncated = result.truncated;
        doc.find.current = find::match_at_or_after(&doc.find.matches, doc.find.anchor);
        let selection = doc
            .find
            .current
            .and_then(|current| doc.find.matches.get(current))
            .cloned();
        if select
            && self.files_is_shown(index)
            && let Some(range) = selection
        {
            self.files_push_selection(range.end, range.start);
        }
        self.files_push_find_status(index);
        self.files_push_find_marks(index, /*force*/ true);
    }

    pub(super) fn files_find_step(&mut self, forward: bool) {
        let Some(index) = self.files_active_index() else {
            return;
        };
        let Some(doc) = text_doc_at(&mut self.tabs, index) else {
            return;
        };
        if !doc.find.open {
            self.files_open_find();
            return;
        }
        let count = doc.find.matches.len();
        if count == 0 {
            if !doc.find.query.is_empty() {
                self.files_run_find(index, /*select*/ true);
            }
            return;
        }
        let next = match doc.find.current {
            Some(current) if forward => (current + 1) % count,
            Some(current) => (current + count - 1) % count,
            None => find::match_at_or_after(&doc.find.matches, doc.find.anchor).unwrap_or(0),
        };
        doc.find.current = Some(next);
        let range = doc.find.matches[next].clone();
        doc.find.anchor = range.start;
        if self.files_is_shown(index) {
            self.files_push_selection(range.end, range.start);
        }
        self.files_push_find_status(index);
        self.files_push_find_marks(index, /*force*/ true);
    }

    fn files_push_find_status(&mut self, index: usize) {
        if !self.files_is_shown(index) {
            return;
        }
        let state = self.window.global::<FilesState>();
        if let Some(doc) = text_doc_at(&mut self.tabs, index) {
            let (status, no_match) = doc.find_status();
            state.set_find_status(status.into());
            state.set_find_no_match(no_match);
        }
    }

    /// The view scrolled: lines `first..=last` (zero-based) are visible.
    pub(super) fn files_viewport_changed(&mut self, first: i32, last: i32) {
        let Some(index) = self
            .files_active_index()
            .filter(|&index| self.files_is_shown(index))
        else {
            return;
        };
        let Some(doc) = text_doc_at(&mut self.tabs, index) else {
            return;
        };
        let first = usize::try_from(first).unwrap_or(0);
        let last = usize::try_from(last).unwrap_or(0).max(first);
        doc.view_lines = (first, last);
        self.files_push_find_marks(index, /*force*/ false);
    }

    /// Highlights every match near the visible lines of tab `index`.
    ///
    /// Only the unwrapped view can place highlights: it measures the text
    /// before each match on its line, and every line has the same height.
    /// With wrapping on, where lines break is not known outside the text
    /// widget, so only the current match is shown (as the selection). Without
    /// `force`, highlights that still cover the visible lines are kept.
    pub(super) fn files_push_find_marks(&mut self, index: usize, force: bool) {
        if !self.files_is_shown(index) {
            return;
        }
        let state = self.window.global::<FilesState>();
        let Some(doc) = text_doc_at(&mut self.tabs, index) else {
            return;
        };
        let wanted = doc.find.open
            && !doc.wrap
            && !doc.find.query.is_empty()
            && !doc.find.matches.is_empty()
            && doc.shows_text();
        if !wanted {
            if doc.find.marked_lines.take().is_some() || force {
                state.set_find_marks(ModelRc::default());
            }
            return;
        }
        let (first, last) = doc.view_lines;
        if !force
            && doc
                .find
                .marked_lines
                .as_ref()
                .is_some_and(|marked| marked.start <= first && last < marked.end)
        {
            return;
        }
        let lines = first.saturating_sub(MARK_MARGIN_LINES)..last + 1 + MARK_MARGIN_LINES;
        let marks = find::marks_in_lines(
            &doc.text,
            &doc.line_starts,
            &doc.find.matches,
            doc.find.current,
            lines.clone(),
            MAX_MARKS,
        );
        doc.find.marked_lines = Some(lines);
        let rows: Vec<FindMark> = marks
            .into_iter()
            .map(|mark| FindMark {
                line: to_i32(mark.line),
                prefix: mark.prefix.into(),
                text: mark.text.into(),
                current: mark.current,
            })
            .collect();
        state.set_find_marks(ModelRc::from(Rc::new(VecModel::from(rows))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn loaded(text_content: &str, total_bytes: u64, complete: bool) -> TextDoc {
        let mut doc = TextDoc::new(PathBuf::from("/tmp/x.txt"), None);
        doc.apply(DocRead {
            canonical: PathBuf::from("/tmp/x.txt"),
            total_bytes,
            modified_ms: 1,
            binary: false,
            executable: false,
            decoded: Some(Decoded::new(
                text_content.to_string(),
                text_content.len() as u64,
                complete,
            )),
        });
        doc
    }

    #[test]
    fn go_to_line_stops_loading_when_a_load_adds_nothing() {
        // 2 of 40 MiB loaded; line 500000 is further on.
        let mut doc = loaded("a\nb\n", 40 * 1024 * 1024, /*complete*/ false);
        doc.pending_line = Some(500_000);
        assert_eq!(doc.pending_line_step(), PendingLineStep::LoadMore);
        doc.loading = true;
        assert_eq!(doc.pending_line_step(), PendingLineStep::Wait);
        // The file was deleted: the load failed and kept the old text.
        doc.loading = false;
        doc.fail(LoadError::Missing);
        assert!(doc.error.is_none());
        assert_eq!(doc.pending_line_step(), PendingLineStep::Reveal);
        // A new request may try again, and keeps going while loads grow.
        doc.auto_load_from = None;
        assert_eq!(doc.pending_line_step(), PendingLineStep::LoadMore);
        doc.loaded_bytes += text::CHUNK_BYTES;
        assert_eq!(doc.pending_line_step(), PendingLineStep::LoadMore);
        doc.pending_line = None;
        assert_eq!(doc.pending_line_step(), PendingLineStep::Nothing);
    }

    #[test]
    fn changes_during_a_load_wait_for_it() {
        let mut doc = loaded("a\n", 2, /*complete*/ true);
        assert!(doc.note_change(/*shown*/ true));
        assert!(!doc.stale);
        // "Load more" is running: reload after it instead of replacing it.
        doc.loading = true;
        assert!(!doc.note_change(/*shown*/ true));
        assert!(doc.stale);
        doc.loading = false;
        doc.stale = false;
        assert!(!doc.note_change(/*shown*/ false));
        assert!(doc.stale);
    }

    #[test]
    fn decode_document_detects_binary_and_cuts_chunks() {
        let (binary, decoded) =
            decode_document(b"\0\x01PNG", /*eof*/ true, /*force_text*/ false);
        assert!(binary);
        assert!(decoded.is_none());

        let (binary, decoded) =
            decode_document(b"\0ok", /*eof*/ true, /*force_text*/ true);
        assert!(binary);
        assert_eq!(
            decoded.map(|decoded| decoded.text),
            Some("\0ok".to_string())
        );

        let (_, decoded) = decode_document(
            b"one\ntwo\nthr",
            /*eof*/ false,
            /*force_text*/ false,
        );
        let decoded = decoded.unwrap_or_else(|| panic!("text expected"));
        assert_eq!(decoded.text, "one\ntwo\n");
        assert_eq!(decoded.consumed, 8);
        assert!(!decoded.complete);
        assert_eq!(decoded.line_starts, vec![0, 4, 8]);
    }

    #[test]
    fn details_and_truncation() {
        let doc = loaded("a\nb\n", 4, /*complete*/ true);
        assert_eq!(doc.details(), "4 B · 2 lines");
        assert_eq!(doc.truncation(), (false, String::new(), false));

        let doc = loaded("a\n", 3 * 1024 * 1024, /*complete*/ false);
        assert_eq!(doc.details(), "3.0 MiB · 1 line loaded");
        assert_eq!(
            doc.truncation(),
            (true, "Showing the first 2 B of 3.0 MiB.".to_string(), true)
        );
    }

    #[test]
    fn extents_grow_by_chunks_and_cap() {
        let mut doc = TextDoc::new(PathBuf::from("/x"), None);
        assert_eq!(doc.extent_for(LoadKind::Initial), text::CHUNK_BYTES);
        doc.loaded_bytes = text::CHUNK_BYTES - 10;
        assert_eq!(doc.extent_for(LoadKind::Reload), text::CHUNK_BYTES);
        assert_eq!(doc.extent_for(LoadKind::More), 2 * text::CHUNK_BYTES - 10);
        doc.loaded_bytes = text::MAX_LOADED_BYTES;
        assert_eq!(doc.extent_for(LoadKind::More), text::MAX_LOADED_BYTES);
    }

    #[test]
    fn reload_failures_keep_content() {
        let mut doc = loaded("x\n", 2, /*complete*/ true);
        doc.fail(LoadError::Missing);
        assert!(doc.error.is_none());
        assert!(doc.notice.is_some());
        assert_eq!(doc.text.as_str(), "x\n");

        let mut fresh = TextDoc::new(PathBuf::from("/nope"), None);
        fresh.fail(LoadError::Missing);
        assert_eq!(fresh.error.as_deref(), Some("/nope does not exist."));
    }

    #[test]
    fn identical_reload_keeps_revision() {
        let mut doc = loaded("same\n", 5, /*complete*/ true);
        let revision = doc.revision;
        doc.apply(DocRead {
            canonical: PathBuf::from("/tmp/x.txt"),
            total_bytes: 5,
            modified_ms: 2,
            binary: false,
            executable: false,
            decoded: Some(Decoded::new(
                "same\n".to_string(),
                /*consumed*/ 5,
                /*complete*/ true,
            )),
        });
        assert_eq!(doc.revision, revision);
        assert_eq!(doc.modified_ms, Some(2));
        assert_eq!(doc.gutter.as_str(), "1");
    }

    #[test]
    fn find_status_text() {
        let mut doc = loaded("abc abc", 7, /*complete*/ true);
        assert_eq!(doc.find_status(), (String::new(), false));
        doc.find.query = "zzz".to_string();
        assert_eq!(doc.find_status(), ("No results".to_string(), true));
        doc.find.query = "abc".to_string();
        doc.find.matches = vec![0..3, 4..7];
        assert_eq!(doc.find_status(), ("2 matches".to_string(), false));
        doc.find.current = Some(1);
        doc.find.truncated = true;
        assert_eq!(doc.find_status(), ("2 of 2+".to_string(), false));
    }

    #[test]
    fn read_prefix_reports_eof() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"0123456789")?;
        assert_eq!(read_prefix(&path, 4)?, (b"0123".to_vec(), false));
        assert_eq!(read_prefix(&path, 10)?, (b"0123456789".to_vec(), true));
        assert_eq!(read_prefix(&path, 100)?, (b"0123456789".to_vec(), true));
        Ok(())
    }

    #[test]
    fn local_twin_requires_matching_modification_time() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"abc")?;
        let modified = std::fs::metadata(&path)?
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_millis();
        let modified = i64::try_from(modified).map_err(std::io::Error::other)?;
        let canonical = dunce::canonicalize(&path)?;
        let same = LocalFile::Same {
            len: 3,
            canonical,
            modified_ms: modified,
            executable: false,
        };
        assert_eq!(local_twin(&path, modified, /*trusted*/ false), same);
        // A different file with the same path on another machine.
        assert_eq!(
            local_twin(&path, modified + 1, /*trusted*/ false),
            LocalFile::Changed
        );
        assert_eq!(
            local_twin(
                &dir.path().join("missing"),
                modified,
                /*trusted*/ false
            ),
            LocalFile::Absent
        );
        assert_eq!(
            local_twin(dir.path(), modified, /*trusted*/ false),
            LocalFile::Absent
        );
        // The embedded server's files are this machine's: a file written
        // between the two looks is still read locally, at its new time.
        assert_eq!(local_twin(&path, modified - 5, /*trusted*/ true), same);
        Ok(())
    }

    #[test]
    fn text_tabs_load_from_memory_in_chunks() {
        let mut doc = TextDoc::in_memory("Reply · repo".to_string(), "one\ntwo\n".to_string());
        assert!(doc.is_memory());
        assert_eq!(doc.title(), "Reply · repo (text)");
        // Never mistaken for a file tab, whatever the path.
        assert!(!doc.shows_path(Path::new("")));
        let read = read_memory("one\ntwo\n", text::CHUNK_BYTES);
        doc.apply(read);
        assert_eq!(doc.text.as_str(), "one\ntwo\n");
        assert!(doc.complete);
        assert_eq!(doc.details(), "8 B · 2 lines");
        assert_eq!(doc.truncation(), (false, String::new(), false));

        // A text longer than one chunk shows a prefix cut after a line.
        let long = "abcd\n".repeat(10);
        let read = read_memory(&long, 12);
        assert_eq!(read.total_bytes, 50);
        let decoded = read
            .decoded
            .as_ref()
            .map(|decoded| (decoded.text.as_str(), decoded.complete));
        assert_eq!(decoded, Some(("abcd\nabcd\n", false)));

        // Replacing the text starts over.
        doc.scroll = (3.0, 4.0);
        doc.replace_memory("new".to_string());
        assert_eq!(doc.loaded_bytes, 0);
        assert_eq!(doc.scroll, (0.0, 0.0));
        assert_eq!(
            doc.memory.as_ref().map(|memory| memory.text.to_string()),
            Some("new".to_string())
        );
    }

    #[test]
    fn file_tabs_are_titled_by_file_name() {
        let doc = TextDoc::new(PathBuf::from("/repo/src/lib.rs"), /*line*/ None);
        assert!(!doc.is_memory());
        assert_eq!(doc.title(), "lib.rs");
        assert!(doc.shows_path(Path::new("/repo/src/lib.rs")));
    }

    #[test]
    fn not_found_messages() {
        assert!(is_not_found_message(
            "No such file or directory (os error 2)"
        ));
        assert!(is_not_found_message(
            "The system cannot find the path specified. (os error 3)"
        ));
        assert!(!is_not_found_message("Permission denied (os error 13)"));
        assert!(!is_not_found_message(
            "the embedded app-server is not running"
        ));
    }
}
