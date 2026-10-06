//! The cross-tab mailbox: every message sent between tabs, by agents or by
//! the user, with its delivery status and reply.
//!
//! GUI.md proposed SQLite. The mailbox is a few hundred small records that
//! are only ever appended or updated by this process, so it is stored as a
//! bounded JSON Lines log at `$CODEX_HOME/gui/mailbox.jsonl` instead: no
//! schema, no migrations, trivially inspectable. Each line is a full entry;
//! a status change appends the entry again and the last line for an id
//! wins. The log is rewritten with the newest [`MAILBOX_CAPACITY`] entries
//! once it grows to [`COMPACT_AT_LINES`] lines.
//!
//! All file I/O runs on a single writer task (ordered, off the UI thread).
//! The log is loaded lazily on first use; that load is the writer task's
//! first operation, so appends made meanwhile can never be lost or reordered.

use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc;

/// Entries kept in memory and after compaction.
pub(crate) const MAILBOX_CAPACITY: usize = 500;
/// Line count at which the log file is compacted.
pub(crate) const COMPACT_AT_LINES: usize = 2 * MAILBOX_CAPACITY;
/// Message and reply texts are stored truncated to this many bytes.
pub(crate) const MAX_STORED_TEXT_BYTES: usize = 8 * 1024;

const MAILBOX_DIR: &str = "gui";
const MAILBOX_FILE: &str = "mailbox.jsonl";

/// Who wrote a message.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MailKind {
    /// Sent by an agent through `send_message_to_thread`.
    Agent,
    /// Forwarded by the user with "Send to tab…".
    User,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MailStatus {
    /// In the target's `thread/queue`, not started yet.
    Queued,
    /// Submitted directly as user input (start or steer).
    Sent,
    /// The target started the turn that carries it.
    Delivered,
    /// The target finished that turn and its answer was returned.
    Replied,
    /// Delivery failed; see `error`.
    Failed,
}

impl MailStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sent => "sent",
            Self::Delivered => "delivered",
            Self::Replied => "replied",
            Self::Failed => "failed",
        }
    }
}

impl MailKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::User => "user",
        }
    }
}

/// One cross-tab message (one JSONL line).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct MailboxEntry {
    pub(crate) id: String,
    pub(crate) from_thread_id: String,
    pub(crate) from_title: String,
    pub(crate) to_thread_id: String,
    pub(crate) to_title: String,
    pub(crate) text: String,
    /// Unix seconds.
    pub(crate) timestamp: i64,
    pub(crate) kind: MailKind,
    pub(crate) status: MailStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reply: Option<String>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) replied_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

/// In-memory mailbox: the newest entries, oldest first, unique by id.
#[derive(Debug)]
pub(crate) struct Mailbox {
    entries: VecDeque<MailboxEntry>,
    capacity: usize,
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::with_capacity(MAILBOX_CAPACITY)
    }
}

impl Mailbox {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// Replaces the entry with the same id in place, or appends it and drops
    /// the oldest entries beyond capacity.
    pub(crate) fn upsert(&mut self, entry: MailboxEntry) {
        if let Some(existing) = self.get_mut(&entry.id) {
            *existing = entry;
            return;
        }
        self.entries.push_back(entry);
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    pub(crate) fn get_mut(&mut self, id: &str) -> Option<&mut MailboxEntry> {
        // Updates almost always target recent entries.
        self.entries.iter_mut().rev().find(|entry| entry.id == id)
    }

    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &MailboxEntry> {
        self.entries.iter()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn snapshot(&self) -> Vec<MailboxEntry> {
        self.entries.iter().cloned().collect()
    }

    /// Merges entries loaded from disk, which predate everything recorded
    /// in memory. Entries already in memory win.
    pub(crate) fn merge_loaded(&mut self, loaded: Vec<MailboxEntry>) {
        let recent = std::mem::take(&mut self.entries);
        for entry in loaded {
            self.upsert(entry);
        }
        for entry in recent {
            self.upsert(entry);
        }
    }

    /// The newest `limit` messages sent to `thread_id`, oldest first.
    pub(crate) fn received_by(&self, thread_id: &str, limit: usize) -> Vec<&MailboxEntry> {
        let mut received: Vec<&MailboxEntry> = self
            .entries
            .iter()
            .rev()
            .filter(|entry| entry.to_thread_id == thread_id)
            .take(limit)
            .collect();
        received.reverse();
        received
    }
}

/// Result of reading the log file.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct LoadedLog {
    /// Unique entries, oldest first, at most `capacity`.
    pub(crate) entries: Vec<MailboxEntry>,
    /// Lines in the file, including superseded and unreadable ones.
    pub(crate) lines: usize,
    pub(crate) skipped: usize,
}

pub(crate) fn mailbox_path(codex_home: &Path) -> PathBuf {
    codex_home.join(MAILBOX_DIR).join(MAILBOX_FILE)
}

/// Parses log text; unreadable lines are skipped.
pub(crate) fn parse_log(text: &str, capacity: usize) -> LoadedLog {
    let mut mailbox = Mailbox::with_capacity(capacity);
    let mut lines = 0;
    let mut skipped = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        lines += 1;
        match serde_json::from_str::<MailboxEntry>(line) {
            Ok(entry) => mailbox.upsert(entry),
            Err(_) => skipped += 1,
        }
    }
    LoadedLog {
        entries: mailbox.entries.into(),
        lines,
        skipped,
    }
}

/// Reads the log; a missing file is an empty mailbox. A log another user
/// could read (created by an older build) is made private first.
pub(crate) fn load_log(path: &Path, capacity: usize) -> std::io::Result<LoadedLog> {
    match restrict_permissions(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(%err, path = %path.display(), "could not make the cross-tab mailbox private")
        }
    }
    match std::fs::read(path) {
        Ok(bytes) => Ok(parse_log(&String::from_utf8_lossy(&bytes), capacity)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(LoadedLog::default()),
        Err(err) => Err(err),
    }
}

/// Messages between agents often quote code, file contents or secrets, so
/// the log is readable by its owner only (like `history.jsonl`).
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o700))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Creates the mailbox folder, private to its owner.
fn create_parent(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent)
}

/// Options for a log file that only its owner can read.
fn private_file_options() -> std::fs::OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = std::fs::OpenOptions::new();
        options.mode(0o600);
        options
    }
    #[cfg(not(unix))]
    std::fs::OpenOptions::new()
}

pub(crate) fn append_log(path: &Path, entries: &[MailboxEntry]) -> std::io::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    create_parent(path)?;
    let mut buffer = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut buffer, entry).map_err(std::io::Error::other)?;
        buffer.push(b'\n');
    }
    let mut file = private_file_options()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(&buffer)?;
    file.flush()
}

/// Atomically replaces the log with `entries` (temp file + rename).
pub(crate) fn rewrite_log(path: &Path, entries: &[MailboxEntry]) -> std::io::Result<()> {
    create_parent(path)?;
    let mut buffer = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut buffer, entry).map_err(std::io::Error::other)?;
        buffer.push(b'\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    // A leftover temp file could carry wider permissions; start fresh so
    // the private mode applies.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let mut file = private_file_options()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    file.write_all(&buffer)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

/// Truncates `text` to at most `max` bytes on a character boundary.
pub(crate) fn truncate_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut cut = max.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

pub(crate) enum StoreOp {
    Append(MailboxEntry),
    Rewrite(Vec<MailboxEntry>),
}

/// Applies ops in order, batching consecutive appends into one write.
pub(crate) fn apply_ops(path: &Path, ops: Vec<StoreOp>) -> std::io::Result<()> {
    let mut pending: Vec<MailboxEntry> = Vec::new();
    for op in ops {
        match op {
            StoreOp::Append(entry) => pending.push(entry),
            StoreOp::Rewrite(entries) => {
                append_log(path, &std::mem::take(&mut pending))?;
                rewrite_log(path, &entries)?;
            }
        }
    }
    append_log(path, &pending)
}

/// Handle to the writer task that owns the log file.
pub(crate) struct MailboxStore {
    tx: mpsc::UnboundedSender<StoreOp>,
}

impl MailboxStore {
    /// Starts the writer task. Its first job is loading the log, reported
    /// through `on_loaded` before any queued op runs.
    pub(crate) fn start(
        runtime: &tokio::runtime::Handle,
        path: PathBuf,
        on_loaded: impl FnOnce(std::io::Result<LoadedLog>) + Send + 'static,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<StoreOp>();
        runtime.spawn(async move {
            let load_path = path.clone();
            let loaded =
                tokio::task::spawn_blocking(move || load_log(&load_path, MAILBOX_CAPACITY))
                    .await
                    .unwrap_or_else(|err| Err(std::io::Error::other(err)));
            on_loaded(loaded);
            while let Some(op) = rx.recv().await {
                let mut ops = vec![op];
                while let Ok(op) = rx.try_recv() {
                    ops.push(op);
                }
                let write_path = path.clone();
                let result = tokio::task::spawn_blocking(move || apply_ops(&write_path, ops))
                    .await
                    .unwrap_or_else(|err| Err(std::io::Error::other(err)));
                if let Err(err) = result {
                    tracing::warn!(%err, path = %path.display(), "failed to write the cross-tab mailbox");
                }
            }
        });
        Self { tx }
    }

    pub(crate) fn send(&self, op: StoreOp) {
        if self.tx.send(op).is_err() {
            tracing::warn!("cross-tab mailbox writer stopped; change not saved");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn entry(id: &str, to: &str, status: MailStatus) -> MailboxEntry {
        MailboxEntry {
            id: id.to_string(),
            from_thread_id: "from".to_string(),
            from_title: "From tab".to_string(),
            to_thread_id: to.to_string(),
            to_title: "To tab".to_string(),
            text: format!("message {id}"),
            timestamp: 1_700_000_000,
            kind: MailKind::Agent,
            status,
            reply: None,
            replied_at: None,
            error: None,
        }
    }

    #[test]
    fn upsert_replaces_by_id_and_evicts_oldest() {
        let mut mailbox = Mailbox::with_capacity(2);
        mailbox.upsert(entry("a", "t", MailStatus::Queued));
        mailbox.upsert(entry("b", "t", MailStatus::Queued));
        mailbox.upsert(entry("a", "t", MailStatus::Delivered));
        assert_eq!(mailbox.len(), 2);
        assert_eq!(
            mailbox
                .iter()
                .map(|e| (e.id.as_str(), e.status))
                .collect::<Vec<_>>(),
            vec![("a", MailStatus::Delivered), ("b", MailStatus::Queued)]
        );
        mailbox.upsert(entry("c", "t", MailStatus::Queued));
        assert_eq!(
            mailbox.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c"]
        );
    }

    #[test]
    fn merge_keeps_memory_entries_newest() {
        let mut mailbox = Mailbox::default();
        mailbox.upsert(entry("new", "t", MailStatus::Queued));
        mailbox.upsert(entry("both", "t", MailStatus::Replied));
        mailbox.merge_loaded(vec![
            entry("old", "t", MailStatus::Delivered),
            entry("both", "t", MailStatus::Queued),
        ]);
        assert_eq!(
            mailbox
                .iter()
                .map(|e| (e.id.as_str(), e.status))
                .collect::<Vec<_>>(),
            vec![
                ("old", MailStatus::Delivered),
                ("both", MailStatus::Replied),
                ("new", MailStatus::Queued),
            ]
        );
    }

    #[test]
    fn received_by_returns_newest_messages_oldest_first() {
        let mut mailbox = Mailbox::default();
        for id in ["1", "2", "3", "4"] {
            mailbox.upsert(entry(
                id,
                if id == "3" { "other" } else { "me" },
                MailStatus::Queued,
            ));
        }
        assert_eq!(
            mailbox
                .received_by("me", 2)
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            vec!["2", "4"]
        );
    }

    #[test]
    fn log_round_trips_through_append_load_and_compaction() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = mailbox_path(dir.path());
        assert_eq!(load_log(&path, 3)?, LoadedLog::default());

        let mut replied = entry("a", "t", MailStatus::Replied);
        replied.reply = Some("done".to_string());
        replied.replied_at = Some(1_700_000_100);
        apply_ops(
            &path,
            vec![
                StoreOp::Append(entry("a", "t", MailStatus::Queued)),
                StoreOp::Append(entry("b", "t", MailStatus::Queued)),
                StoreOp::Append(replied.clone()),
            ],
        )?;
        // A torn or foreign line does not lose the rest of the log.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)?
            .write_all(b"{not json\n")?;
        apply_ops(
            &path,
            vec![StoreOp::Append(entry("c", "t", MailStatus::Failed))],
        )?;

        let loaded = load_log(&path, 3)?;
        assert_eq!(loaded.lines, 5);
        assert_eq!(loaded.skipped, 1);
        assert_eq!(
            loaded.entries,
            vec![
                replied,
                entry("b", "t", MailStatus::Queued),
                entry("c", "t", MailStatus::Failed)
            ]
        );

        // Capacity keeps the newest entries.
        let small = load_log(&path, 2)?;
        assert_eq!(
            small
                .entries
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c"]
        );

        apply_ops(
            &path,
            vec![
                StoreOp::Append(entry("d", "t", MailStatus::Queued)),
                StoreOp::Rewrite(small.entries),
                StoreOp::Append(entry("e", "t", MailStatus::Queued)),
            ],
        )?;
        let compacted = load_log(&path, 10)?;
        assert_eq!(compacted.lines, 3);
        assert_eq!(compacted.skipped, 0);
        assert_eq!(
            compacted
                .entries
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c", "e"]
        );
        assert!(!path.with_extension("jsonl.tmp").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn the_log_is_private_to_its_owner() -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| -> std::io::Result<u32> {
            Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
        };
        let dir = tempfile::tempdir()?;
        let path = mailbox_path(dir.path());
        append_log(&path, &[entry("a", "t", MailStatus::Queued)])?;
        assert_eq!(mode(&path)?, 0o600);
        assert_eq!(mode(path.parent().unwrap_or(dir.path()))?, 0o700);
        rewrite_log(&path, &[entry("b", "t", MailStatus::Queued)])?;
        assert_eq!(mode(&path)?, 0o600);

        // A log written by an older build is tightened when loaded.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
        let loaded = load_log(&path, 10)?;
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(mode(&path)?, 0o600);
        Ok(())
    }

    #[test]
    fn entries_serialize_compactly() -> serde_json::Result<()> {
        let line = serde_json::to_string(&entry("a", "t", MailStatus::Queued))?;
        assert_eq!(
            line,
            r#"{"id":"a","from_thread_id":"from","from_title":"From tab","to_thread_id":"t","to_title":"To tab","text":"message a","timestamp":1700000000,"kind":"agent","status":"queued"}"#
        );
        Ok(())
    }

    #[test]
    fn truncate_bytes_respects_char_boundaries() {
        assert_eq!(truncate_bytes("short", 10), "short");
        let truncated = truncate_bytes(&"é".repeat(10), 8);
        assert!(truncated.len() <= 8, "{truncated}");
        assert!(truncated.ends_with('…'));
    }
}
