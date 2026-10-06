//! Per-tab transcript store: ordered entries, their rendered rows, and the
//! Slint model kept in sync with them.
//!
//! An [`Entry`] is one transcript element (a thread item, a notice, a turn
//! marker). Each entry caches its rendered [`Block`]s; the model holds the
//! converted rows of all entries back to back. Re-rendering an entry diffs
//! its new blocks against the cached ones and only touches rows that
//! changed, so streaming a long message updates one or two rows per frame.
//!
//! Item entries are keyed by item id (local user echoes by their client id
//! until the server confirms them), which deduplicates live events, history
//! pages, and lag resyncs. Legacy threads report other ids for the same
//! items when read back (see `history`); [`Transcript::adopt_snapshot`]
//! records those ids as aliases of the live entries.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::Hash;
use std::hash::Hasher;
use std::rc::Rc;

use codex_app_server_protocol::CollabAgentToolCallStatus;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallStatus;
use codex_app_server_protocol::HookRunSummary;
use codex_app_server_protocol::ImageReference;
use codex_app_server_protocol::McpToolCallStatus;
use codex_app_server_protocol::PatchApplyStatus;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnPlanStepStatus;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use slint::ModelRc;

use super::NoticeKind;
use super::blocks::Block;
use super::blocks::BlockKind;
use super::blocks::Tone;
use super::history::HistoryState;
use super::history::TrimBoundary;
use super::history::has_synthetic_legacy_id;
use super::markdown::BlockStyle;
use super::model::TranscriptModel;
use super::output::LiveOutput;
use super::render::RenderContext;
use super::render::reasoning_header;
use super::render::render_entry;
use super::streaming::MarkdownStream;
use crate::ui::BlockData;

/// Streaming / live state of an in-progress item.
#[derive(Clone, Debug, Default)]
pub(crate) enum Live {
    #[default]
    None,
    Markdown(Box<MarkdownStream>),
    Output(LiveOutput),
}

/// A thread item and its UI state.
#[derive(Clone, Debug)]
pub(crate) struct ItemEntry {
    pub(crate) item: ThreadItem,
    pub(crate) live: Live,
    /// Toggle ids of expanded sub-elements (see `render::TOGGLE_MAIN`).
    pub(crate) expanded: BTreeSet<usize>,
    /// A user message shown before the server confirmed it.
    pub(crate) local_echo: bool,
    /// A local echo whose message could not be sent (or queued).
    pub(crate) unsent: bool,
    /// `item/completed` arrived (or the item came from history).
    pub(crate) completed: bool,
    /// Latest progress text (MCP progress, terminal stdin).
    pub(crate) progress: Option<String>,
    /// Created from a live event rather than a history page: a legacy read
    /// reports the item under another id.
    pub(crate) from_event: bool,
}

impl ItemEntry {
    /// An item from a live event.
    fn from_event(item: ThreadItem, live: Live, completed: bool) -> Self {
        Self {
            item: slim_item(item),
            live,
            expanded: BTreeSet::new(),
            local_echo: false,
            unsent: false,
            completed,
            progress: None,
            from_event: true,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TurnEndEntry {
    pub(crate) status: TurnStatus,
    pub(crate) duration_ms: Option<i64>,
    pub(crate) error: Option<String>,
    pub(crate) error_details: Option<String>,
    /// The failure was already shown by an `error` notification.
    pub(crate) error_reported: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct PlanUpdateEntry {
    pub(crate) explanation: Option<String>,
    pub(crate) steps: Vec<(String, TurnPlanStepStatus)>,
}

#[derive(Clone, Debug)]
pub(crate) struct HookEntry {
    pub(crate) run: HookRunSummary,
}

/// A titled Markdown card added by the GUI (see
/// [`crate::app::AppController::transcript_push_card`]).
#[derive(Clone, Debug)]
pub(crate) struct CardEntry {
    pub(crate) title: String,
    pub(crate) markdown: String,
}

#[derive(Clone, Debug)]
pub(crate) enum Body {
    Item(Box<ItemEntry>),
    Notice {
        kind: NoticeKind,
        title: String,
        text: String,
    },
    /// Transient "reconnecting" status for an `error` with `will_retry`.
    Retry {
        message: String,
        details: Option<String>,
    },
    TurnEnd(TurnEndEntry),
    PlanUpdate(PlanUpdateEntry),
    Hook(HookEntry),
    Card(CardEntry),
}

#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub(crate) key: String,
    pub(crate) turn_id: Option<String>,
    pub(crate) body: Body,
    pub(crate) blocks: Vec<Block>,
    pub(crate) bytes: usize,
}

impl Entry {
    fn new(key: String, turn_id: Option<String>, body: Body) -> Self {
        Self {
            key,
            turn_id,
            body,
            blocks: Vec::new(),
            bytes: 0,
        }
    }

    /// An item loaded from history (already complete).
    pub(crate) fn from_history(turn_id: String, item: ThreadItem) -> Self {
        let key = item.id().to_string();
        let mut item = ItemEntry::from_event(item, Live::None, /*completed*/ true);
        item.from_event = false;
        Self::new(key, Some(turn_id), Body::Item(Box::new(item)))
    }

    pub(crate) fn item(&self) -> Option<&ItemEntry> {
        match &self.body {
            Body::Item(item) => Some(item),
            _ => None,
        }
    }

    fn item_mut(&mut self) -> Option<&mut ItemEntry> {
        match &mut self.body {
            Body::Item(item) => Some(item),
            _ => None,
        }
    }

    /// A completed item that came from the server (not a local echo).
    pub(crate) fn is_server_item(&self) -> bool {
        self.item().is_some_and(|item| !item.local_echo)
    }

    /// Content only this window knows (notices, cards, plan updates, hook
    /// runs, messages that were not sent): history pages cannot bring it
    /// back once trimmed.
    fn is_gui_only(&self) -> bool {
        match &self.body {
            Body::Notice { .. } | Body::Card(_) | Body::PlanUpdate(_) | Body::Hook(_) => true,
            Body::Item(item) => item.local_echo && item.unsent,
            Body::Retry { .. } | Body::TurnEnd(_) => false,
        }
    }

    /// Never trimmed: still streaming, or a message waiting for the server.
    fn is_pending(&self) -> bool {
        self.item().is_some_and(|item| {
            if item.local_echo {
                !item.unsent
            } else {
                !item.completed
            }
        })
    }

    /// The first row is the header of a cross-tab message card.
    fn is_cross_tab_card(&self) -> bool {
        self.blocks
            .first()
            .is_some_and(|block| block.kind == BlockKind::CardHeader && !block.target.is_empty())
    }

    pub(crate) fn turn_end(turn: &Turn, error_reported: bool) -> Self {
        Self::new(
            turn_end_key(&turn.id),
            Some(turn.id.clone()),
            Body::TurnEnd(TurnEndEntry {
                status: turn.status.clone(),
                duration_ms: turn.duration_ms,
                error: turn.error.as_ref().map(|error| error.message.clone()),
                error_details: turn
                    .error
                    .as_ref()
                    .and_then(|error| error.additional_details.clone()),
                error_reported,
            }),
        )
    }
}

pub(crate) fn turn_end_key(turn_id: &str) -> String {
    format!("turn-{turn_id}")
}

fn row_id(key: &str, local: usize) -> String {
    format!("{key}#{local}")
}

/// Splits a row id into the entry key and the row index within the entry.
pub(crate) fn split_row_id(id: &str) -> Option<(&str, usize)> {
    let (key, local) = id.rsplit_once('#')?;
    Some((key, local.parse().ok()?))
}

/// Prefix of an inline `data:` URL kept by [`slim_item`].
fn slim_data_url(url: &mut String) {
    if url.starts_with("data:")
        && let Some(comma) = url.find(',')
    {
        url.truncate(comma + 1);
        url.shrink_to_fit();
    }
}

/// Drops payloads the transcript never shows or exports (generated image
/// data, inline image and audio data, raw web search results), which can be
/// megabytes per item and would otherwise stay alive with the tab.
pub(crate) fn slim_item(mut item: ThreadItem) -> ThreadItem {
    match &mut item {
        ThreadItem::ImageGeneration(generation) => generation.result = String::new(),
        ThreadItem::WebSearch(search) => search.results = None,
        ThreadItem::UserMessage { content, .. } => {
            for input in content {
                match input {
                    UserInput::Image {
                        image: ImageReference::Inline { url },
                        ..
                    }
                    | UserInput::Audio { url } => slim_data_url(url),
                    _ => {}
                }
            }
        }
        ThreadItem::DynamicToolCall {
            content_items: Some(items),
            ..
        } => {
            for item in items {
                match item {
                    DynamicToolCallOutputContentItem::InputImage { image_url: url }
                    | DynamicToolCallOutputContentItem::InputAudio { audio_url: url } => {
                        slim_data_url(url);
                    }
                    DynamicToolCallOutputContentItem::InputText { .. } => {}
                }
            }
        }
        ThreadItem::McpToolCall {
            result: Some(result),
            ..
        } => {
            for content in &mut result.content {
                if matches!(
                    content.get("type").and_then(serde_json::Value::as_str),
                    Some("image" | "audio")
                ) && let Some(data) = content.get_mut("data")
                {
                    *data = serde_json::Value::String(String::new());
                }
            }
        }
        _ => {}
    }
    item
}

/// Approximate bytes an item keeps alive outside its rendered rows.
fn item_bytes(item: &ThreadItem) -> usize {
    match item {
        ThreadItem::UserMessage { content, .. } => content
            .iter()
            .map(|input| match input {
                UserInput::Text { text, .. } => text.len(),
                UserInput::Image {
                    image: ImageReference::Inline { url },
                    ..
                }
                | UserInput::Audio { url } => url.len(),
                _ => 64,
            })
            .sum(),
        ThreadItem::AgentMessage { text, .. } | ThreadItem::Plan { text, .. } => text.len(),
        ThreadItem::Reasoning {
            summary, content, ..
        } => summary.iter().chain(content).map(String::len).sum(),
        ThreadItem::CommandExecution {
            command,
            aggregated_output,
            ..
        } => command.len() + aggregated_output.as_ref().map_or(0, String::len),
        ThreadItem::FileChange { changes, .. } => changes
            .iter()
            .map(|change| change.diff.len() + change.path.len())
            .sum(),
        ThreadItem::McpToolCall {
            arguments, result, ..
        } => {
            arguments.to_string().len()
                + result.as_ref().map_or(0, |result| {
                    serde_json::to_string(&result.content).map_or(0, |s| s.len())
                })
        }
        ThreadItem::DynamicToolCall {
            arguments,
            content_items,
            ..
        } => {
            arguments.to_string().len()
                + content_items
                    .iter()
                    .flatten()
                    .map(|item| match item {
                        DynamicToolCallOutputContentItem::InputText { text } => text.len(),
                        DynamicToolCallOutputContentItem::InputImage { image_url: url }
                        | DynamicToolCallOutputContentItem::InputAudio { audio_url: url } => {
                            url.len()
                        }
                    })
                    .sum::<usize>()
        }
        ThreadItem::ImageGeneration(generation) => {
            128 + generation.result.len()
                + generation.revised_prompt.as_ref().map_or(0, String::len)
        }
        ThreadItem::ExitedReviewMode { review, .. } => review.len(),
        _ => 128,
    }
}

fn live_bytes(live: &Live) -> usize {
    match live {
        Live::None => 0,
        Live::Markdown(stream) => stream.raw().len() * 2,
        Live::Output(output) => output.len(),
    }
}

fn entry_bytes(entry: &Entry, blocks: &[Block]) -> usize {
    let own = match &entry.body {
        Body::Item(item) => item_bytes(&item.item) + live_bytes(&item.live),
        Body::Notice { text, .. } => text.len(),
        Body::Card(card) => card.title.len() + card.markdown.len(),
        _ => 64,
    };
    own + blocks.iter().map(Block::byte_len).sum::<usize>()
}

/// Identity of a user, agent or reasoning item that does not depend on its
/// id: legacy reads give these items other ids than their live events.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Signature {
    kind: u8,
    /// Hash of the turn id and the item's text.
    hash: u64,
}

fn signature(turn_id: &str, item: &ThreadItem) -> Option<Signature> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    turn_id.hash(&mut hasher);
    let kind = match item {
        ThreadItem::UserMessage { content, .. } => {
            for input in content {
                if let UserInput::Text { text, .. } = input {
                    text.hash(&mut hasher);
                }
            }
            0
        }
        ThreadItem::AgentMessage { text, .. } => {
            text.hash(&mut hasher);
            1
        }
        ThreadItem::Reasoning {
            summary, content, ..
        } => {
            summary.hash(&mut hasher);
            content.hash(&mut hasher);
            2
        }
        _ => return None,
    };
    Some(Signature {
        kind,
        hash: hasher.finish(),
    })
}

/// Where a trimmed GUI-only entry goes back when history is paged in again.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Anchor {
    /// After this server item; `past_turn_end` when the entry also followed
    /// the end-of-turn marker that came after the item.
    After {
        key: String,
        signature: Option<Signature>,
        past_turn_end: bool,
    },
    /// Right before this server item.
    Before {
        key: String,
        signature: Option<Signature>,
    },
}

/// A GUI-only entry dropped by [`Transcript::trim_front`].
#[derive(Clone, Debug)]
struct Stashed {
    anchor: Anchor,
    entry: Entry,
}

/// Trimmed GUI-only entries remembered per tab; the oldest are forgotten.
const MAX_STASHED: usize = 256;

/// Per-tab transcript.
pub(crate) struct Transcript {
    model: Rc<TranscriptModel>,
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
    rows: usize,
    bytes: usize,
    notice_seq: u64,
    retry_key: Option<String>,
    /// Thread warnings already shown (servers repeat some on every turn).
    warnings_shown: std::collections::HashSet<String>,
    /// GUI-only entries that were trimmed, restored next to their anchor
    /// item when history pages bring it back.
    stashed: Vec<Stashed>,
    /// Ids a legacy read reported for entries created from live events
    /// (snapshot id → entry key), and the reverse.
    aliases: HashMap<String, String>,
    alias_of: HashMap<String, String>,
    /// Turns whose failure an `error` notification already showed.
    reported_turn_errors: HashSet<String>,
    pub(crate) history: HistoryState,
    /// Headline of the reasoning item currently streaming, for the status line.
    pub(crate) activity: Option<String>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub(crate) fn new() -> Self {
        Self {
            model: Rc::new(TranscriptModel::default()),
            entries: Vec::new(),
            index: HashMap::new(),
            rows: 0,
            bytes: 0,
            notice_seq: 0,
            retry_key: None,
            warnings_shown: std::collections::HashSet::new(),
            stashed: Vec::new(),
            aliases: HashMap::new(),
            alias_of: HashMap::new(),
            reported_turn_errors: HashSet::new(),
            history: HistoryState::default(),
            activity: None,
        }
    }

    pub(crate) fn model(&self) -> ModelRc<BlockData> {
        ModelRc::from(self.model.clone())
    }

    pub(crate) fn rows(&self) -> usize {
        self.rows
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub(crate) fn entry(&self, key: &str) -> Option<&Entry> {
        self.index.get(key).map(|&index| &self.entries[index])
    }

    pub(crate) fn contains(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }

    /// Index of the entry for item `id`, also when `id` is the id a legacy
    /// read reported for an entry created from a live event.
    fn find(&self, id: &str) -> Option<usize> {
        self.index.get(id).copied().or_else(|| {
            self.aliases
                .get(id)
                .and_then(|key| self.index.get(key))
                .copied()
        })
    }

    /// Forgets the aliases of entry `key` (removed).
    fn forget_aliases(&mut self, key: &str) {
        if let Some(id) = self.alias_of.remove(key) {
            self.aliases.remove(&id);
        }
    }

    // ----- row bookkeeping --------------------------------------------------

    fn row_start(&self, index: usize) -> usize {
        if index * 2 > self.entries.len() {
            let after: usize = self.entries[index..]
                .iter()
                .map(|entry| entry.blocks.len())
                .sum();
            self.rows - after
        } else {
            self.entries[..index]
                .iter()
                .map(|entry| entry.blocks.len())
                .sum()
        }
    }

    fn rebuild_index(&mut self) {
        self.index = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.key.clone(), index))
            .collect();
    }

    /// Re-renders entry `index` and updates only the rows that changed.
    fn rerender(&mut self, index: usize, ctx: RenderContext<'_>) {
        let blocks = render_entry(&self.entries[index], ctx);
        self.replace_blocks(index, blocks, /*force*/ false);
    }

    fn replace_blocks(&mut self, index: usize, new: Vec<Block>, force: bool) {
        let start = self.row_start(index);
        let model = self.model.clone();
        let entry = &mut self.entries[index];
        let old = std::mem::take(&mut entry.blocks);
        let common = old.len().min(new.len());
        for local in 0..common {
            if force || old[local] != new[local] {
                model.set(start + local, new[local].to_row(&row_id(&entry.key, local)));
            }
        }
        if new.len() > old.len() {
            let rows = new[common..]
                .iter()
                .enumerate()
                .map(|(offset, block)| block.to_row(&row_id(&entry.key, common + offset)))
                .collect();
            model.insert(start + common, rows);
        } else if old.len() > new.len() {
            model.remove(start + common, old.len() - new.len());
        }
        self.rows = self.rows + new.len() - old.len();
        let bytes = entry_bytes(entry, &new);
        self.bytes = self.bytes + bytes - entry.bytes.min(self.bytes);
        entry.bytes = bytes;
        entry.blocks = new;
    }

    /// Inserts rendered entries before entry `at`.
    fn insert_entries(&mut self, at: usize, mut entries: Vec<Entry>, ctx: RenderContext<'_>) {
        if entries.is_empty() {
            return;
        }
        let at = at.min(self.entries.len());
        let start = self.row_start(at);
        let mut rows = Vec::new();
        for entry in &mut entries {
            let blocks = render_entry(entry, ctx);
            rows.extend(
                blocks
                    .iter()
                    .enumerate()
                    .map(|(local, block)| block.to_row(&row_id(&entry.key, local))),
            );
            entry.bytes = entry_bytes(entry, &blocks);
            self.rows += blocks.len();
            self.bytes += entry.bytes;
            entry.blocks = blocks;
        }
        self.model.insert(start, rows);
        let appended = at == self.entries.len();
        let count = entries.len();
        self.entries.splice(at..at, entries);
        if appended {
            for index in at..at + count {
                self.index.insert(self.entries[index].key.clone(), index);
            }
        } else {
            self.rebuild_index();
        }
    }

    fn push_entry(&mut self, entry: Entry, ctx: RenderContext<'_>) -> usize {
        let at = self.entries.len();
        self.insert_entries(at, vec![entry], ctx);
        at
    }

    fn remove_entry(&mut self, index: usize) -> Entry {
        let start = self.row_start(index);
        let entry = self.entries.remove(index);
        self.model.remove(start, entry.blocks.len());
        self.rows -= entry.blocks.len();
        self.bytes -= entry.bytes.min(self.bytes);
        self.rebuild_index();
        self.forget_aliases(&entry.key);
        entry
    }

    // ----- live events --------------------------------------------------------

    /// Applies a thread-scoped notification; returns true when rows changed.
    /// `reported_error` is the tab's `(turn_id, message)` of the last `error`
    /// notification, used to avoid repeating it when `turn/completed`
    /// reports the same failure.
    pub(crate) fn apply(
        &mut self,
        notification: &ServerNotification,
        reported_error: &mut Option<(String, String)>,
        ctx: RenderContext<'_>,
    ) -> bool {
        match notification {
            ServerNotification::ItemStarted(started) => {
                self.clear_retry();
                self.upsert_item(
                    &started.turn_id,
                    started.item.clone(),
                    /*completed*/ false,
                    ctx,
                );
                true
            }
            ServerNotification::ItemCompleted(completed) => {
                self.upsert_item(
                    &completed.turn_id,
                    completed.item.clone(),
                    /*completed*/ true,
                    ctx,
                );
                true
            }
            ServerNotification::AgentMessageDelta(delta) => {
                self.clear_retry();
                self.stream_delta(
                    &delta.turn_id,
                    &delta.item_id,
                    &delta.delta,
                    /*plan*/ false,
                    ctx,
                )
            }
            ServerNotification::PlanDelta(delta) => {
                self.stream_delta(
                    &delta.turn_id,
                    &delta.item_id,
                    &delta.delta,
                    /*plan*/ true,
                    ctx,
                )
            }
            ServerNotification::ReasoningSummaryTextDelta(delta) => self.reasoning_delta(
                &delta.turn_id,
                &delta.item_id,
                delta.summary_index,
                Some(&delta.delta),
                /*raw*/ false,
                ctx,
            ),
            ServerNotification::ReasoningSummaryPartAdded(part) => self.reasoning_delta(
                &part.turn_id,
                &part.item_id,
                part.summary_index,
                None,
                /*raw*/ false,
                ctx,
            ),
            ServerNotification::ReasoningTextDelta(delta) => self.reasoning_delta(
                &delta.turn_id,
                &delta.item_id,
                delta.content_index,
                Some(&delta.delta),
                /*raw*/ true,
                ctx,
            ),
            ServerNotification::CommandExecutionOutputDelta(delta) => {
                self.with_item(&delta.item_id, ctx, |item| {
                    if item.completed {
                        return false;
                    }
                    match &mut item.live {
                        Live::Output(output) => output.push(&delta.delta),
                        live => {
                            let mut output = LiveOutput::default();
                            output.push(&delta.delta);
                            *live = Live::Output(output);
                        }
                    }
                    true
                })
            }
            ServerNotification::TerminalInteraction(interaction) => {
                self.with_item(&interaction.item_id, ctx, |item| {
                    item.progress = Some(interaction.stdin.clone());
                    true
                })
            }
            ServerNotification::McpToolCallProgress(progress) => {
                self.with_item(&progress.item_id, ctx, |item| {
                    item.progress = Some(progress.message.clone());
                    true
                })
            }
            ServerNotification::FileChangePatchUpdated(updated) => {
                self.with_item(&updated.item_id, ctx, |item| {
                    if let ThreadItem::FileChange { changes, .. } = &mut item.item
                        && !item.completed
                    {
                        changes.clone_from(&updated.changes);
                        return true;
                    }
                    false
                })
            }
            ServerNotification::TurnStarted(_) => {
                self.clear_retry();
                self.activity = None;
                false
            }
            ServerNotification::TurnCompleted(completed) => {
                self.turn_completed(&completed.turn, reported_error, ctx);
                true
            }
            ServerNotification::TurnPlanUpdated(plan) => {
                let body = Body::PlanUpdate(PlanUpdateEntry {
                    explanation: plan
                        .explanation
                        .clone()
                        .filter(|text| !text.trim().is_empty()),
                    steps: plan
                        .plan
                        .iter()
                        .map(|step| (step.step.clone(), step.status))
                        .collect(),
                });
                self.upsert_simple(
                    format!("plan-{}", plan.turn_id),
                    Some(&plan.turn_id),
                    body,
                    ctx,
                );
                true
            }
            ServerNotification::Error(error) => {
                if error.will_retry {
                    let key = format!("retry-{}", error.turn_id);
                    let body = Body::Retry {
                        message: error.error.message.clone(),
                        details: error.error.additional_details.clone(),
                    };
                    self.upsert_simple(key.clone(), Some(&error.turn_id), body, ctx);
                    self.retry_key = Some(key);
                } else {
                    self.clear_retry();
                    *reported_error = Some((error.turn_id.clone(), error.error.message.clone()));
                    let mut text = error.error.message.clone();
                    if let Some(details) = error
                        .error
                        .additional_details
                        .as_deref()
                        .filter(|details| !details.trim().is_empty())
                    {
                        text.push_str("\n\n");
                        text.push_str(details);
                    }
                    self.push_notice(NoticeKind::Error, "Error", &text, ctx);
                }
                true
            }
            ServerNotification::HookStarted(hook) => {
                self.upsert_hook(&hook.run, hook.turn_id.as_deref(), ctx);
                true
            }
            ServerNotification::HookCompleted(hook) => {
                self.upsert_hook(&hook.run, hook.turn_id.as_deref(), ctx);
                true
            }
            ServerNotification::ModelRerouted(rerouted) => {
                self.push_notice(
                    NoticeKind::Warning,
                    "Model switched",
                    &format!(
                        "{} → {} ({:?})",
                        rerouted.from_model, rerouted.to_model, rerouted.reason
                    ),
                    ctx,
                );
                true
            }
            ServerNotification::ModelVerification(verification) => {
                let kinds: Vec<String> = verification
                    .verifications
                    .iter()
                    .map(|kind| format!("{kind:?}"))
                    .collect();
                self.push_notice(
                    NoticeKind::Info,
                    "Model verification",
                    &format!("Additional verification is required: {}", kinds.join(", ")),
                    ctx,
                );
                true
            }
            ServerNotification::AuthRecoveryStarted(recovery)
            | ServerNotification::AuthRecoveryCompleted(recovery) => {
                self.push_notice(NoticeKind::Info, &recovery.provider, &recovery.message, ctx);
                true
            }
            ServerNotification::Warning(warning) if warning.thread_id.is_some() => {
                if !self.warnings_shown.insert(warning.message.clone()) {
                    return false;
                }
                self.push_notice(NoticeKind::Warning, "", &warning.message, ctx);
                true
            }
            ServerNotification::GuardianWarning(warning) => {
                if warning
                    .message
                    .starts_with("Automatic approval review approved (")
                    || !self.warnings_shown.insert(warning.message.clone())
                {
                    return false;
                }
                self.push_notice(NoticeKind::Warning, "", &warning.message, ctx);
                true
            }
            _ => false,
        }
    }

    fn clear_retry(&mut self) {
        if let Some(key) = self.retry_key.take()
            && let Some(&index) = self.index.get(&key)
        {
            self.remove_entry(index);
        }
    }

    /// Applies `f` to item `item_id` and re-renders when it returns true.
    fn with_item(
        &mut self,
        item_id: &str,
        ctx: RenderContext<'_>,
        f: impl FnOnce(&mut ItemEntry) -> bool,
    ) -> bool {
        let Some(&index) = self.index.get(item_id) else {
            return false;
        };
        let Some(item) = self.entries[index].item_mut() else {
            return false;
        };
        if !f(item) {
            return false;
        }
        self.rerender(index, ctx);
        true
    }

    fn upsert_simple(
        &mut self,
        key: String,
        turn_id: Option<&str>,
        body: Body,
        ctx: RenderContext<'_>,
    ) {
        match self.index.get(&key) {
            Some(&index) => {
                self.entries[index].body = body;
                self.rerender(index, ctx);
            }
            None => {
                self.push_entry(Entry::new(key, turn_id.map(str::to_string), body), ctx);
            }
        }
    }

    fn upsert_hook(&mut self, run: &HookRunSummary, turn_id: Option<&str>, ctx: RenderContext<'_>) {
        self.upsert_simple(
            format!("hook-{}", run.id),
            turn_id,
            Body::Hook(HookEntry { run: run.clone() }),
            ctx,
        );
    }

    /// Adds an info / warning / error row.
    pub(crate) fn push_notice(
        &mut self,
        kind: NoticeKind,
        title: &str,
        text: &str,
        ctx: RenderContext<'_>,
    ) {
        self.notice_seq += 1;
        let key = format!("notice-{}", self.notice_seq);
        self.push_entry(
            Entry::new(
                key,
                None,
                Body::Notice {
                    kind,
                    title: title.to_string(),
                    text: text.to_string(),
                },
            ),
            ctx,
        );
    }

    /// Shows a notice in the row `key`, replacing the text it showed before
    /// (for errors that can repeat, like failed history loads).
    pub(crate) fn set_notice(
        &mut self,
        key: &str,
        kind: NoticeKind,
        title: &str,
        text: &str,
        ctx: RenderContext<'_>,
    ) {
        let body = Body::Notice {
            kind,
            title: title.to_string(),
            text: text.to_string(),
        };
        self.upsert_simple(key.to_string(), /*turn_id*/ None, body, ctx);
    }

    /// Removes the notice row `key`, also when it was trimmed.
    pub(crate) fn clear_notice(&mut self, key: &str) {
        if let Some(&index) = self.index.get(key) {
            self.remove_entry(index);
        }
        self.stashed.retain(|stashed| stashed.entry.key != key);
    }

    /// Adds a titled Markdown card.
    pub(crate) fn push_card(&mut self, title: &str, markdown: &str, ctx: RenderContext<'_>) {
        self.notice_seq += 1;
        let key = format!("card-{}", self.notice_seq);
        self.push_entry(
            Entry::new(
                key,
                None,
                Body::Card(CardEntry {
                    title: title.to_string(),
                    markdown: markdown.to_string(),
                }),
            ),
            ctx,
        );
    }

    /// Re-renders cross-tab message cards, whose source links depend on
    /// which threads are open (`ctx.open_threads`). Unchanged rows are not
    /// touched.
    pub(crate) fn refresh_cross_tab_links(&mut self, ctx: RenderContext<'_>) {
        let cards: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.is_cross_tab_card())
            .map(|(index, _)| index)
            .collect();
        for index in cards {
            self.rerender(index, ctx);
        }
    }

    /// Shows a user message before the server confirms it.
    pub(crate) fn push_local_user_message(
        &mut self,
        client_id: &str,
        input: &[UserInput],
        ctx: RenderContext<'_>,
    ) {
        if self.index.contains_key(client_id) {
            return;
        }
        let item = ThreadItem::UserMessage {
            id: client_id.to_string(),
            client_id: Some(client_id.to_string()),
            content: input.to_vec(),
        };
        let mut item = ItemEntry::from_event(item, Live::None, /*completed*/ false);
        item.local_echo = true;
        let entry = Entry::new(client_id.to_string(), None, Body::Item(Box::new(item)));
        self.push_entry(entry, ctx);
    }

    /// The local echo `client_id`, while the server has not confirmed it.
    fn local_echo_index(&self, client_id: &str) -> Option<usize> {
        let index = *self.index.get(client_id)?;
        self.entries[index]
            .item()
            .is_some_and(|item| item.local_echo)
            .then_some(index)
    }

    /// Marks the local echo `client_id` as not sent (the request failed).
    /// It stays visible, so the text is not lost, but is no longer pending:
    /// it can be trimmed, is not exported, and later identical messages are
    /// not matched to it. Returns false when there is no such echo.
    pub(crate) fn mark_echo_unsent(&mut self, client_id: &str, ctx: RenderContext<'_>) -> bool {
        let Some(index) = self.local_echo_index(client_id) else {
            return false;
        };
        if let Some(item) = self.entries[index].item_mut() {
            item.unsent = true;
        }
        self.rerender(index, ctx);
        true
    }

    /// Removes the local echo `client_id` (its queued message was deleted).
    pub(crate) fn remove_echo(&mut self, client_id: &str) -> bool {
        let Some(index) = self.local_echo_index(client_id) else {
            return false;
        };
        self.remove_entry(index);
        true
    }

    /// The local echo a server user message confirms, if any.
    fn local_echo_for(&self, item: &ThreadItem) -> Option<usize> {
        let ThreadItem::UserMessage {
            client_id, content, ..
        } = item
        else {
            return None;
        };
        if let Some(client_id) = client_id
            && let Some(index) = self.local_echo_index(client_id)
        {
            return Some(index);
        }
        // Servers that do not echo the client id: match the oldest pending
        // echo with identical content. Echoes that were not sent never
        // reach the server, so they cannot be what it confirms.
        self.entries.iter().position(|entry| {
            entry.item().is_some_and(|item| {
                item.local_echo
                    && !item.unsent
                    && matches!(&item.item, ThreadItem::UserMessage { content: echoed, .. } if echoed == content)
            })
        })
    }

    fn upsert_item(
        &mut self,
        turn_id: &str,
        item: ThreadItem,
        completed: bool,
        ctx: RenderContext<'_>,
    ) {
        let item = slim_item(item);
        let key = item.id().to_string();
        if let ThreadItem::Reasoning { summary, .. } = &item
            && !completed
        {
            self.activity = reasoning_header(summary).or_else(|| self.activity.clone());
        }
        if !self.index.contains_key(&key)
            && let Some(echo) = self.local_echo_for(&item)
        {
            // Confirm the echo: re-key it and move it to the end when other
            // output arrived in between (queued or steered input).
            let mut entry = self.remove_entry(echo);
            entry.key = key;
            entry.turn_id = Some(turn_id.to_string());
            if let Some(echoed) = entry.item_mut() {
                echoed.item = item;
                echoed.local_echo = false;
                echoed.unsent = false;
                echoed.completed = completed;
            }
            entry.blocks.clear();
            entry.bytes = 0;
            self.push_entry(entry, ctx);
            return;
        }
        match self.index.get(&key).copied() {
            Some(index) => {
                let Some(existing) = self.entries[index].item_mut() else {
                    return;
                };
                if existing.completed && !completed {
                    // A late `item/started` must not undo a completion.
                    return;
                }
                if completed {
                    existing.live = Live::None;
                    existing.completed = true;
                    existing.item = item;
                } else {
                    let had_stream = matches!(existing.live, Live::Markdown(_));
                    existing.item = item;
                    if !had_stream {
                        existing.live = initial_live(&existing.item, ctx);
                    }
                }
                self.entries[index].turn_id = Some(turn_id.to_string());
                self.rerender(index, ctx);
            }
            None => {
                let live = if completed {
                    Live::None
                } else {
                    initial_live(&item, ctx)
                };
                let entry = Entry::new(
                    key,
                    Some(turn_id.to_string()),
                    Body::Item(Box::new(ItemEntry::from_event(item, live, completed))),
                );
                self.push_entry(entry, ctx);
            }
        }
    }

    fn stream_delta(
        &mut self,
        turn_id: &str,
        item_id: &str,
        delta: &str,
        plan: bool,
        ctx: RenderContext<'_>,
    ) -> bool {
        if !self.index.contains_key(item_id) {
            // The `item/started` was dropped (lag); start from the deltas.
            let item = if plan {
                ThreadItem::Plan {
                    id: item_id.to_string(),
                    text: String::new(),
                }
            } else {
                ThreadItem::AgentMessage {
                    id: item_id.to_string(),
                    text: String::new(),
                    phase: None,
                    memory_citation: None,
                    delivery: None,
                    questions: None,
                }
            };
            self.upsert_item(turn_id, item, /*completed*/ false, ctx);
        }
        self.with_item(item_id, ctx, |item| {
            if item.completed {
                return false;
            }
            match &mut item.live {
                Live::Markdown(stream) => stream.push(delta),
                _ => false,
            }
        })
    }

    fn reasoning_delta(
        &mut self,
        turn_id: &str,
        item_id: &str,
        index: i64,
        delta: Option<&str>,
        raw: bool,
        ctx: RenderContext<'_>,
    ) -> bool {
        if !self.index.contains_key(item_id) {
            let item = ThreadItem::Reasoning {
                id: item_id.to_string(),
                summary: Vec::new(),
                content: Vec::new(),
            };
            self.upsert_item(turn_id, item, /*completed*/ false, ctx);
        }
        let Ok(index) = usize::try_from(index) else {
            return false;
        };
        let mut header = None;
        let changed = self.with_item(item_id, ctx, |item| {
            if item.completed {
                return false;
            }
            let ThreadItem::Reasoning {
                summary, content, ..
            } = &mut item.item
            else {
                return false;
            };
            let parts = if raw { content } else { summary };
            if parts.len() <= index {
                parts.resize(index + 1, String::new());
            }
            if let Some(delta) = delta {
                parts[index].push_str(delta);
            }
            if !raw {
                header = reasoning_header(parts);
            }
            true
        });
        if header.is_some() {
            self.activity = header;
        }
        changed
    }

    fn turn_completed(
        &mut self,
        turn: &Turn,
        reported_error: &mut Option<(String, String)>,
        ctx: RenderContext<'_>,
    ) {
        self.clear_retry();
        self.activity = None;
        // Items that never completed: freeze what was streamed.
        let pending: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.turn_id.as_deref() == Some(turn.id.as_str())
                    && entry
                        .item()
                        .is_some_and(|item| !item.completed && !item.local_echo)
            })
            .map(|(index, _)| index)
            .collect();
        for index in pending {
            if let Some(item) = self.entries[index].item_mut() {
                finalize_unfinished(item, &turn.status);
            }
            self.rerender(index, ctx);
        }
        // `turn/completed` carries the final message; render it when its
        // item events were lost.
        for item in &turn.items {
            if matches!(item, ThreadItem::AgentMessage { .. }) && !self.contains(item.id()) {
                self.upsert_item(&turn.id, item.clone(), /*completed*/ true, ctx);
            }
        }
        let reported = turn.status == TurnStatus::Failed
            && match (reported_error.as_ref(), turn.error.as_ref()) {
                (Some((turn_id, message)), Some(error)) => {
                    turn_id == &turn.id && message == &error.message
                }
                (Some((turn_id, _)), None) => turn_id == &turn.id,
                _ => false,
            };
        if reported_error
            .as_ref()
            .is_some_and(|(turn_id, _)| turn_id == &turn.id)
        {
            *reported_error = None;
        }
        // Remember the turn so its end marker comes back when trimmed rows
        // are paged in again (history pages only describe older turns).
        let mut metadata = turn.clone();
        metadata.items.clear();
        self.history.turns.insert(metadata.id.clone(), metadata);
        if reported {
            self.reported_turn_errors.insert(turn.id.clone());
        }
        let entry = Entry::turn_end(turn, reported);
        match self.index.get(&entry.key).copied() {
            Some(index) => {
                self.entries[index].body = entry.body;
                self.rerender(index, ctx);
            }
            None => {
                self.push_entry(entry, ctx);
            }
        }
    }

    // ----- user actions --------------------------------------------------------

    /// Toggles the expandable element shown in row `row_id`.
    pub(crate) fn toggle(&mut self, row_id: &str, ctx: RenderContext<'_>) -> bool {
        let Some((key, local)) = split_row_id(row_id) else {
            return false;
        };
        let Some(&index) = self.index.get(key) else {
            return false;
        };
        let Some(toggle) = self.entries[index]
            .blocks
            .get(local)
            .and_then(|block| block.toggle)
        else {
            return false;
        };
        let Some(item) = self.entries[index].item_mut() else {
            return false;
        };
        if !item.expanded.remove(&toggle) {
            item.expanded.insert(toggle);
        }
        self.rerender(index, ctx);
        true
    }

    /// The entry and block shown in row `row_id`.
    pub(crate) fn block_for_row(&self, row_id: &str) -> Option<(&Entry, &Block)> {
        let (key, local) = split_row_id(row_id)?;
        let entry = self.entry(key)?;
        Some((entry, entry.blocks.get(local)?))
    }

    // ----- history ----------------------------------------------------------------

    /// Inserts history items (chronological) before everything shown.
    /// Items already present are skipped. Returns the number of entries added.
    pub(crate) fn prepend_history(
        &mut self,
        items: Vec<(String, ThreadItem)>,
        turns: &HashMap<String, Turn>,
        ctx: RenderContext<'_>,
    ) -> usize {
        let next_existing_turn = self.entries.first().and_then(|entry| entry.turn_id.clone());
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut entries = Vec::new();
        let count = items.len();
        for (position, (turn_id, item)) in items.iter().enumerate() {
            let key = item.id();
            let duplicate = self.find(key).is_some()
                || seen.contains(key)
                || matches!(item, ThreadItem::UserMessage { client_id: Some(client), .. } if self.index.contains_key(client));
            if !duplicate {
                seen.insert(key.to_string());
                entries.push(Entry::from_history(turn_id.clone(), item.clone()));
            }
            let next_turn = if position + 1 < count {
                Some(items[position + 1].0.clone())
            } else {
                next_existing_turn.clone()
            };
            if next_turn.as_deref() != Some(turn_id.as_str())
                && let Some(turn) = turns.get(turn_id)
                && turn.status != TurnStatus::InProgress
            {
                let end = Entry::turn_end(turn, self.reported_turn_errors.contains(turn_id));
                if !self.index.contains_key(&end.key) && seen.insert(end.key.clone()) {
                    entries.push(end);
                }
            }
        }
        let added = entries.len();
        self.insert_entries(0, entries, ctx);
        self.restore_stashed(ctx);
        added
    }

    /// Reconciles the newest items (chronological) after events were
    /// dropped: existing items take the server's version, missing ones are
    /// inserted in order.
    pub(crate) fn resync(&mut self, items: Vec<(String, ThreadItem)>, ctx: RenderContext<'_>) {
        let mut pending: Vec<Entry> = Vec::new();
        for (turn_id, item) in items {
            let item = slim_item(item);
            let mut key = item.id().to_string();
            let aliased = self.aliases.get(&key).cloned();
            let existing = self.find(&key).or_else(|| self.local_echo_for(&item));
            match existing {
                Some(mut index) => {
                    if !pending.is_empty() {
                        let count = pending.len();
                        self.insert_entries(index, std::mem::take(&mut pending), ctx);
                        index += count;
                    }
                    // An entry known under a live id keeps that id: its
                    // later events still refer to it.
                    let item = match aliased {
                        Some(live_key) => {
                            key = live_key;
                            with_item_id(item, &key)
                        }
                        None => item,
                    };
                    let entry = &mut self.entries[index];
                    let renamed = entry.key != key;
                    if let Some(existing) = entry.item_mut()
                        && (!existing.completed || existing.item != item || renamed)
                    {
                        existing.item = item;
                        existing.live = Live::None;
                        existing.completed = true;
                        existing.local_echo = false;
                        existing.unsent = false;
                        entry.turn_id = Some(turn_id);
                        if renamed {
                            entry.key = key;
                            self.rebuild_index();
                            let blocks = render_entry(&self.entries[index], ctx);
                            self.replace_blocks(index, blocks, /*force*/ true);
                        } else {
                            self.rerender(index, ctx);
                        }
                    }
                }
                None => pending.push(Entry::from_history(turn_id, item)),
            }
        }
        let end = self.entries.len();
        self.insert_entries(end, pending, ctx);
        self.restore_stashed(ctx);
    }

    /// Turns with user, agent or reasoning entries created from live events
    /// whose legacy ids are not known yet.
    pub(crate) fn live_turns(&self) -> HashSet<String> {
        self.entries
            .iter()
            .filter(|entry| {
                entry.item().is_some_and(|item| {
                    item.from_event && !item.local_echo && has_synthetic_legacy_id(&item.item)
                }) && !self.alias_of.contains_key(&entry.key)
            })
            .filter_map(|entry| entry.turn_id.clone())
            .collect()
    }

    /// Records the ids a legacy read (`snapshot`: user, agent and reasoning
    /// items of the live turns, chronological) reports for entries created
    /// from live events, so history pages and resyncs find those entries
    /// instead of adding them again. Entries keep their keys, which later
    /// events of the same items use.
    ///
    /// Items are matched per turn and kind: equal text first, then by
    /// position from the newest (trimming drops the oldest entries first,
    /// and the legacy reader merges consecutive reasoning items).
    pub(crate) fn adopt_snapshot(&mut self, snapshot: &[(String, ThreadItem)]) {
        type Group = (String, u8);
        let mut candidates: HashMap<Group, Vec<(String, u64)>> = HashMap::new();
        for (turn_id, item) in snapshot {
            let id = item.id();
            if self.index.contains_key(id) || self.aliases.contains_key(id) {
                continue;
            }
            if let Some(signature) = signature(turn_id, item) {
                candidates
                    .entry((turn_id.clone(), signature.kind))
                    .or_default()
                    .push((id.to_string(), signature.hash));
            }
        }
        if candidates.is_empty() {
            return;
        }
        let mut live: HashMap<Group, Vec<(String, u64)>> = HashMap::new();
        for entry in &self.entries {
            let Some(item) = entry.item() else {
                continue;
            };
            if !item.from_event || item.local_echo || self.alias_of.contains_key(&entry.key) {
                continue;
            }
            if let Some(turn_id) = &entry.turn_id
                && let Some(signature) = signature(turn_id, &item.item)
            {
                live.entry((turn_id.clone(), signature.kind))
                    .or_default()
                    .push((entry.key.clone(), signature.hash));
            }
        }
        for (group, entries) in live {
            let Some(candidates) = candidates.get_mut(&group) else {
                continue;
            };
            let mut unmatched = Vec::new();
            for (key, hash) in entries.into_iter().rev() {
                match candidates
                    .iter()
                    .rposition(|(_, candidate)| *candidate == hash)
                {
                    Some(position) => {
                        let (id, _) = candidates.remove(position);
                        self.add_alias(id, key);
                    }
                    None => unmatched.push(key),
                }
            }
            for key in unmatched {
                let Some((id, _)) = candidates.pop() else {
                    break;
                };
                self.add_alias(id, key);
            }
        }
    }

    fn add_alias(&mut self, id: String, key: String) {
        self.alias_of.insert(key.clone(), id.clone());
        self.aliases.insert(id, key);
    }

    /// Drops the oldest entries until at most `max_rows` rows and
    /// `max_bytes` bytes remain (entries with live state are never
    /// dropped). Returns the newest dropped server item, which is where
    /// reloading older history has to resume, or `None` when nothing was
    /// dropped.
    pub(crate) fn trim_front(
        &mut self,
        max_rows: usize,
        max_bytes: usize,
    ) -> Option<Option<TrimBoundary>> {
        if self.rows <= max_rows && self.bytes <= max_bytes {
            return None;
        }
        let mut rows = 0;
        let mut bytes = 0;
        let mut keep_from = self.entries.len();
        for (index, entry) in self.entries.iter().enumerate().rev() {
            if !entry.is_pending()
                && (rows + entry.blocks.len() > max_rows || bytes + entry.bytes > max_bytes)
            {
                break;
            }
            rows += entry.blocks.len();
            bytes += entry.bytes;
            keep_from = index;
        }
        // Always keep the newest entry so the tab never looks empty.
        keep_from = keep_from.min(self.entries.len().saturating_sub(1));
        if keep_from == 0 {
            return None;
        }
        let newest_dropped = self.entries[..keep_from]
            .iter()
            .rev()
            .find(|entry| entry.is_server_item())
            .map(|entry| TrimBoundary {
                id: self.alias_of.get(&entry.key).unwrap_or(&entry.key).clone(),
                turn_id: entry.turn_id.clone(),
            });
        let dropped_rows: usize = self.entries[..keep_from]
            .iter()
            .map(|entry| entry.blocks.len())
            .sum();
        let dropped_bytes: usize = self.entries[..keep_from]
            .iter()
            .map(|entry| entry.bytes)
            .sum();
        self.stash_gui_only(keep_from);
        let dropped: Vec<Entry> = self.entries.drain(..keep_from).collect();
        for entry in &dropped {
            self.forget_aliases(&entry.key);
        }
        self.model.remove(0, dropped_rows);
        self.rows -= dropped_rows;
        self.bytes -= dropped_bytes.min(self.bytes);
        self.rebuild_index();
        Some(newest_dropped)
    }

    /// Remembers the GUI-only entries among the first `count` entries
    /// (about to be trimmed) with the server item they follow, or else the
    /// one they precede.
    fn stash_gui_only(&mut self, count: usize) {
        let mut previous_item: Option<(String, Option<Signature>)> = None;
        let mut past_turn_end = false;
        let mut orphans: Vec<Entry> = Vec::new();
        for entry in &self.entries[..count] {
            if entry.is_server_item() {
                previous_item = Some((entry.key.clone(), self.anchor_signature(entry)));
                past_turn_end = false;
                continue;
            }
            if matches!(entry.body, Body::TurnEnd(_)) {
                past_turn_end = true;
                continue;
            }
            if !entry.is_gui_only() {
                continue;
            }
            let mut entry = entry.clone();
            entry.blocks.clear();
            entry.bytes = 0;
            match &previous_item {
                Some((key, signature)) => self.stashed.push(Stashed {
                    anchor: Anchor::After {
                        key: key.clone(),
                        signature: *signature,
                        past_turn_end,
                    },
                    entry,
                }),
                None => orphans.push(entry),
            }
        }
        if !orphans.is_empty()
            && let Some(next) = self.entries.iter().find(|entry| entry.is_server_item())
        {
            // Nothing older was shown: anchor before the next server item.
            let anchor = Anchor::Before {
                key: next.key.clone(),
                signature: self.anchor_signature(next),
            };
            self.stashed
                .extend(orphans.into_iter().map(|entry| Stashed {
                    anchor: anchor.clone(),
                    entry,
                }));
        }
        if self.stashed.len() > MAX_STASHED {
            let excess = self.stashed.len() - MAX_STASHED;
            self.stashed.drain(..excess);
        }
    }

    /// What identifies `entry` as an anchor when its id can change: items
    /// from live events of user, agent and reasoning messages come back from
    /// legacy reads under other ids.
    fn anchor_signature(&self, entry: &Entry) -> Option<Signature> {
        let item = entry.item()?;
        if !item.from_event || self.alias_of.contains_key(&entry.key) {
            return None;
        }
        signature(entry.turn_id.as_deref()?, &item.item)
    }

    /// Index of the anchor item `key`, or else of the item with `wanted`
    /// (`by_signature` maps signatures to entry keys; built when needed).
    fn anchor_index(
        &self,
        key: &str,
        wanted: Option<Signature>,
        by_signature: &mut Option<HashMap<Signature, String>>,
    ) -> Option<usize> {
        if let Some(index) = self.find(key) {
            return Some(index);
        }
        let wanted = wanted?;
        let map = by_signature.get_or_insert_with(|| {
            self.entries
                .iter()
                .filter_map(|entry| {
                    let item = entry.item()?;
                    let signature = signature(entry.turn_id.as_deref()?, &item.item)?;
                    Some((signature, entry.key.clone()))
                })
                .collect()
        });
        map.get(&wanted)
            .and_then(|key| self.index.get(key))
            .copied()
    }

    /// Puts stashed entries back once their anchor item is shown again.
    fn restore_stashed(&mut self, ctx: RenderContext<'_>) {
        if self.stashed.is_empty() {
            return;
        }
        let mut waiting = Vec::new();
        // Restored entries are GUI-only and have no signature, so the map
        // stays valid while they are inserted.
        let mut by_signature = None;
        for stashed in std::mem::take(&mut self.stashed) {
            if self.index.contains_key(&stashed.entry.key) {
                continue;
            }
            let at = match &stashed.anchor {
                Anchor::After {
                    key,
                    signature,
                    past_turn_end,
                } => self
                    .anchor_index(key, *signature, &mut by_signature)
                    .map(|index| self.restore_position(index + 1, *past_turn_end)),
                Anchor::Before { key, signature } => {
                    self.anchor_index(key, *signature, &mut by_signature)
                }
            };
            match at {
                Some(at) => self.insert_entries(at, vec![stashed.entry], ctx),
                None => waiting.push(stashed),
            }
        }
        self.stashed = waiting;
    }

    /// Where an entry that followed an item goes: after the GUI-only entries
    /// already restored there, and after the turn's end marker when it
    /// originally came after it.
    fn restore_position(&self, mut at: usize, past_turn_end: bool) -> usize {
        while let Some(entry) = self.entries.get(at) {
            match entry.body {
                Body::Item(_) => break,
                Body::TurnEnd(_) if !past_turn_end => break,
                _ => at += 1,
            }
        }
        at
    }

    // ----- queries ----------------------------------------------------------------

    /// Whether any item of the transcript is still streaming.
    #[cfg(test)]
    pub(crate) fn has_live_items(&self) -> bool {
        self.entries.iter().any(|entry| {
            entry
                .item()
                .is_some_and(|item| !item.completed && !item.local_echo)
        })
    }
}

/// `item` (a user, agent or reasoning message) under the id `id`.
fn with_item_id(mut item: ThreadItem, id: &str) -> ThreadItem {
    match &mut item {
        ThreadItem::UserMessage { id: item_id, .. }
        | ThreadItem::AgentMessage { id: item_id, .. }
        | ThreadItem::Reasoning { id: item_id, .. } => *item_id = id.to_string(),
        _ => {}
    }
    item
}

fn initial_live(item: &ThreadItem, ctx: RenderContext<'_>) -> Live {
    match item {
        ThreadItem::AgentMessage { text, .. } | ThreadItem::Plan { text, .. } => {
            let style = BlockStyle {
                tone: if matches!(item, ThreadItem::Plan { .. }) {
                    Tone::Plan
                } else {
                    Tone::Normal
                },
                message: true,
            };
            let mut stream = MarkdownStream::new(style, ctx.cwd);
            stream.push(text);
            Live::Markdown(Box::new(stream))
        }
        ThreadItem::CommandExecution {
            aggregated_output, ..
        } => {
            let mut output = LiveOutput::default();
            if let Some(text) = aggregated_output {
                output.push(text);
            }
            Live::Output(output)
        }
        _ => Live::None,
    }
}

/// Freezes an item whose turn ended before `item/completed` arrived. The
/// server does not complete tool calls that were running when a turn was
/// interrupted, so their status is settled here.
fn finalize_unfinished(item: &mut ItemEntry, turn_status: &TurnStatus) {
    let succeeded = *turn_status == TurnStatus::Completed;
    match (&mut item.item, std::mem::take(&mut item.live)) {
        (ThreadItem::McpToolCall { status, .. }, _) if *status == McpToolCallStatus::InProgress => {
            *status = if succeeded {
                McpToolCallStatus::Completed
            } else {
                McpToolCallStatus::Failed
            };
        }
        (ThreadItem::DynamicToolCall { status, .. }, _)
            if *status == DynamicToolCallStatus::InProgress =>
        {
            *status = if succeeded {
                DynamicToolCallStatus::Completed
            } else {
                DynamicToolCallStatus::Failed
            };
        }
        (ThreadItem::FileChange { status, .. }, _) if *status == PatchApplyStatus::InProgress => {
            *status = match turn_status {
                TurnStatus::Completed => PatchApplyStatus::Completed,
                // Interrupted while waiting for approval or applying.
                TurnStatus::Interrupted => PatchApplyStatus::Declined,
                _ => PatchApplyStatus::Failed,
            };
        }
        (ThreadItem::CollabAgentToolCall { status, .. }, _)
            if *status == CollabAgentToolCallStatus::InProgress =>
        {
            *status = match turn_status {
                TurnStatus::Completed => CollabAgentToolCallStatus::Completed,
                TurnStatus::Interrupted => CollabAgentToolCallStatus::Interrupted,
                _ => CollabAgentToolCallStatus::Failed,
            };
        }
        (
            ThreadItem::AgentMessage { text, .. } | ThreadItem::Plan { text, .. },
            Live::Markdown(stream),
        ) if text.is_empty() => {
            *text = stream.raw().to_string();
        }
        (
            ThreadItem::CommandExecution {
                status,
                aggregated_output,
                ..
            },
            live,
        ) => {
            if let Live::Output(output) = live
                && aggregated_output.is_none()
                && !output.text().is_empty()
            {
                *aggregated_output = Some(output.text().to_string());
            }
            if *status == CommandExecutionStatus::InProgress {
                *status = if *turn_status == TurnStatus::Completed {
                    CommandExecutionStatus::Completed
                } else {
                    CommandExecutionStatus::Failed
                };
            }
        }
        _ => {}
    }
    item.completed = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::AgentMessageDeltaNotification;
    use codex_app_server_protocol::ErrorNotification;
    use codex_app_server_protocol::ItemCompletedNotification;
    use codex_app_server_protocol::ItemStartedNotification;
    use codex_app_server_protocol::TurnCompletedNotification;
    use codex_app_server_protocol::TurnError;
    use codex_app_server_protocol::TurnItemsView;
    use pretty_assertions::assert_eq;
    use slint::Model;
    use std::path::Path;

    fn ctx() -> RenderContext<'static> {
        RenderContext {
            cwd: Path::new("/repo"),
            show_raw_reasoning: false,
            hide_reasoning: false,
            open_threads: &[],
        }
    }

    fn agent(id: &str, text: &str) -> ThreadItem {
        ThreadItem::AgentMessage {
            id: id.to_string(),
            text: text.to_string(),
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        }
    }

    fn user(id: &str, client: Option<&str>, text: &str) -> ThreadItem {
        ThreadItem::UserMessage {
            id: id.to_string(),
            client_id: client.map(str::to_string),
            content: vec![crate::session::text_input(text)],
        }
    }

    fn started(item: ThreadItem) -> ServerNotification {
        ServerNotification::ItemStarted(ItemStartedNotification {
            item,
            thread_id: "t".to_string(),
            turn_id: "turn-1".to_string(),
            started_at_ms: 0,
        })
    }

    fn completed(item: ThreadItem) -> ServerNotification {
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            item,
            thread_id: "t".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
        })
    }

    fn delta(item: &str, text: &str) -> ServerNotification {
        ServerNotification::AgentMessageDelta(AgentMessageDeltaNotification {
            thread_id: "t".to_string(),
            turn_id: "turn-1".to_string(),
            item_id: item.to_string(),
            delta: text.to_string(),
        })
    }

    fn turn(status: TurnStatus, error: Option<&str>) -> Turn {
        Turn {
            id: "turn-1".to_string(),
            items: Vec::new(),
            items_view: TurnItemsView::NotLoaded,
            status,
            error: error.map(|message| TurnError {
                message: message.to_string(),
                codex_error_info: None,
                additional_details: None,
                misalignment: None,
            }),
            started_at: None,
            completed_at: None,
            duration_ms: Some(2500),
        }
    }

    fn row_ids(transcript: &Transcript) -> Vec<String> {
        (0..transcript.model.row_count())
            .filter_map(|row| transcript.model.id_at(row))
            .collect()
    }

    fn apply(transcript: &mut Transcript, notification: ServerNotification) {
        let mut reported = None;
        transcript.apply(&notification, &mut reported, ctx());
    }

    #[test]
    fn streams_and_completes_agent_messages() {
        let mut transcript = Transcript::new();
        apply(&mut transcript, started(agent("a", "")));
        apply(&mut transcript, delta("a", "# Title\n\nHello"));
        assert_eq!(row_ids(&transcript), vec!["a#0", "a#1"]);
        apply(&mut transcript, delta("a", " world"));
        assert_eq!(transcript.rows(), 2);
        apply(
            &mut transcript,
            completed(agent("a", "# Title\n\nHello world, final.")),
        );
        assert_eq!(transcript.rows(), 2);
        let row = transcript.model.row_data(1).map(|row| row.rich);
        assert_eq!(
            row,
            Some(slint::StyledText::from_markdown("Hello world, final\\.").unwrap_or_default())
        );
        assert_eq!(transcript.model.len(), transcript.rows());
    }

    #[test]
    fn local_echo_is_confirmed_and_moved_to_the_end() {
        let mut transcript = Transcript::new();
        transcript.push_local_user_message("c1", &[crate::session::text_input("hi")], ctx());
        apply(&mut transcript, completed(agent("a", "earlier")));
        apply(&mut transcript, started(user("u1", Some("c1"), "hi")));
        assert_eq!(row_ids(&transcript), vec!["a#0", "u1#0"]);
        // A repeated event for the same item does not duplicate it.
        apply(&mut transcript, completed(user("u1", Some("c1"), "hi")));
        assert_eq!(row_ids(&transcript), vec!["a#0", "u1#0"]);
    }

    #[test]
    fn error_then_failed_turn_is_shown_once() {
        let mut transcript = Transcript::new();
        let mut reported = None;
        transcript.apply(
            &ServerNotification::Error(ErrorNotification {
                error: TurnError {
                    message: "boom".to_string(),
                    codex_error_info: None,
                    additional_details: None,
                    misalignment: None,
                },
                will_retry: false,
                thread_id: "t".to_string(),
                turn_id: "turn-1".to_string(),
            }),
            &mut reported,
            ctx(),
        );
        assert_eq!(reported, Some(("turn-1".to_string(), "boom".to_string())));
        transcript.apply(
            &ServerNotification::TurnCompleted(TurnCompletedNotification {
                thread_id: "t".to_string(),
                turn: turn(TurnStatus::Failed, Some("boom")),
            }),
            &mut reported,
            ctx(),
        );
        let notices: Vec<BlockKind> = transcript
            .entries()
            .iter()
            .flat_map(|entry| entry.blocks.iter().map(|block| block.kind))
            .collect();
        assert_eq!(notices, vec![BlockKind::Notice]);
        assert_eq!(reported, None);
    }

    #[test]
    fn unfinished_items_are_frozen_at_turn_end() {
        let mut transcript = Transcript::new();
        apply(&mut transcript, started(agent("a", "")));
        apply(&mut transcript, delta("a", "partial text"));
        let mut reported = None;
        transcript.apply(
            &ServerNotification::TurnCompleted(TurnCompletedNotification {
                thread_id: "t".to_string(),
                turn: turn(TurnStatus::Interrupted, None),
            }),
            &mut reported,
            ctx(),
        );
        let entry = transcript.entry("a").and_then(Entry::item).map(|item| {
            (
                item.completed,
                matches!(&item.item, ThreadItem::AgentMessage { text, .. } if text == "partial text"),
            )
        });
        assert_eq!(entry, Some((true, true)));
        let separator = transcript
            .entry("turn-turn-1")
            .map(|entry| entry.blocks[0].title.clone());
        assert_eq!(separator, Some("Interrupted after 2s".to_string()));
        assert!(!transcript.has_live_items());
    }

    #[test]
    fn history_is_prepended_without_duplicates() {
        let mut transcript = Transcript::new();
        apply(&mut transcript, completed(agent("live", "now")));
        let mut turns = HashMap::new();
        turns.insert("old".to_string(), {
            let mut turn = turn(TurnStatus::Completed, None);
            turn.id = "old".to_string();
            turn
        });
        let added = transcript.prepend_history(
            vec![
                ("old".to_string(), user("u0", None, "question")),
                ("old".to_string(), agent("a0", "answer")),
                ("turn-1".to_string(), agent("live", "now")),
            ],
            &turns,
            ctx(),
        );
        assert_eq!(added, 3);
        assert_eq!(
            row_ids(&transcript),
            vec!["u0#0", "a0#0", "turn-old#0", "live#0"]
        );
    }

    #[test]
    fn trimming_keeps_the_tail_and_reports_the_resume_point() {
        let mut transcript = Transcript::new();
        for index in 0..10 {
            apply(
                &mut transcript,
                completed(agent(&format!("a{index}"), "text")),
            );
        }
        let resume = transcript.trim_front(3, usize::MAX);
        assert_eq!(
            resume,
            Some(Some(TrimBoundary {
                id: "a6".to_string(),
                turn_id: Some("turn-1".to_string()),
            }))
        );
        assert_eq!(row_ids(&transcript), vec!["a7#0", "a8#0", "a9#0"]);
        assert_eq!(transcript.trim_front(3, usize::MAX), None);
    }

    #[test]
    fn resync_updates_and_inserts_in_order() {
        let mut transcript = Transcript::new();
        apply(&mut transcript, completed(agent("a", "one")));
        apply(&mut transcript, started(agent("c", "")));
        apply(&mut transcript, delta("c", "trunc"));
        transcript.resync(
            vec![
                ("turn-1".to_string(), agent("a", "one")),
                ("turn-1".to_string(), agent("b", "two")),
                ("turn-1".to_string(), agent("c", "truncated no more")),
                ("turn-1".to_string(), agent("d", "four")),
            ],
            ctx(),
        );
        assert_eq!(row_ids(&transcript), vec!["a#0", "b#0", "c#0", "d#0"]);
        assert!(!transcript.has_live_items());
    }

    fn notice_keys(transcript: &Transcript) -> Vec<String> {
        transcript
            .entries()
            .iter()
            .map(|entry| entry.key.clone())
            .collect()
    }

    #[test]
    fn trimmed_notices_come_back_with_their_history() {
        let mut transcript = Transcript::new();
        apply(&mut transcript, completed(agent("a0", "zero")));
        transcript.push_notice(NoticeKind::Warning, "", "careful", ctx());
        apply(&mut transcript, completed(agent("a1", "one")));
        transcript.push_card("Recap", "So far **so good**.", ctx());
        apply(&mut transcript, completed(agent("a2", "two")));
        assert_eq!(
            notice_keys(&transcript),
            vec!["a0", "notice-1", "a1", "card-2", "a2"]
        );
        // Trim everything but the newest item.
        let resume = transcript.trim_front(1, usize::MAX).flatten();
        assert_eq!(resume.map(|boundary| boundary.id), Some("a1".to_string()));
        assert_eq!(notice_keys(&transcript), vec!["a2"]);
        // Older history is paged back in: the notice and the card return
        // next to the items they followed.
        transcript.prepend_history(
            vec![
                ("turn-1".to_string(), agent("a0", "zero")),
                ("turn-1".to_string(), agent("a1", "one")),
            ],
            &HashMap::new(),
            ctx(),
        );
        assert_eq!(
            notice_keys(&transcript),
            vec!["a0", "notice-1", "a1", "card-2", "a2"]
        );
        assert_eq!(transcript.model.len(), transcript.rows());
        assert!(transcript.stashed.is_empty());
    }

    #[test]
    fn notices_before_any_item_return_before_the_next_item() {
        let mut transcript = Transcript::new();
        transcript.push_notice(NoticeKind::Error, "History", "could not load", ctx());
        apply(&mut transcript, completed(agent("a0", "zero")));
        apply(&mut transcript, completed(agent("a1", "one")));
        transcript.trim_front(1, usize::MAX);
        assert_eq!(notice_keys(&transcript), vec!["a1"]);
        transcript.prepend_history(
            vec![("turn-1".to_string(), agent("a0", "zero"))],
            &HashMap::new(),
            ctx(),
        );
        assert_eq!(notice_keys(&transcript), vec!["notice-1", "a0", "a1"]);
    }

    #[test]
    fn notices_after_a_turn_end_stay_after_it() {
        let mut transcript = Transcript::new();
        apply(&mut transcript, completed(agent("a0", "zero")));
        let mut reported = None;
        transcript.apply(
            &ServerNotification::TurnCompleted(TurnCompletedNotification {
                thread_id: "t".to_string(),
                turn: turn(TurnStatus::Completed, None),
            }),
            &mut reported,
            ctx(),
        );
        transcript.push_notice(NoticeKind::Info, "", "after the turn", ctx());
        apply(
            &mut transcript,
            ServerNotification::ItemCompleted(ItemCompletedNotification {
                item: agent("a1", "one"),
                thread_id: "t".to_string(),
                turn_id: "turn-2".to_string(),
                completed_at_ms: 0,
            }),
        );
        assert_eq!(
            notice_keys(&transcript),
            vec!["a0", "turn-turn-1", "notice-1", "a1"]
        );
        transcript.trim_front(1, usize::MAX);
        let mut turns = HashMap::new();
        turns.insert("turn-1".to_string(), turn(TurnStatus::Completed, None));
        transcript.prepend_history(
            vec![("turn-1".to_string(), agent("a0", "zero"))],
            &turns,
            ctx(),
        );
        assert_eq!(
            notice_keys(&transcript),
            vec!["a0", "turn-turn-1", "notice-1", "a1"]
        );
    }

    #[test]
    fn cross_tab_links_follow_open_tabs() {
        let mut transcript = Transcript::new();
        let text = crate::xtab::tools::wrap_agent_message(
            "source", "repo-a", "hello", /*reply_expected*/ false,
        );
        apply(&mut transcript, completed(user("u1", None, &text)));
        let detail = |transcript: &Transcript| {
            transcript
                .model
                .row_data(0)
                .map(|row| row.detail.to_string())
        };
        assert_eq!(detail(&transcript), Some("Open thread".to_string()));
        let open = ["source".to_string()];
        transcript.refresh_cross_tab_links(RenderContext {
            open_threads: &open,
            ..ctx()
        });
        assert_eq!(detail(&transcript), Some("Go to tab".to_string()));
    }

    fn completed_in(turn_id: &str, item: ThreadItem) -> ServerNotification {
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            item,
            thread_id: "t".to_string(),
            turn_id: turn_id.to_string(),
            completed_at_ms: 0,
        })
    }

    fn turn_completed(transcript: &mut Transcript, id: &str, status: TurnStatus) {
        let mut turn = turn(status, None);
        turn.id = id.to_string();
        let mut reported = None;
        transcript.apply(
            &ServerNotification::TurnCompleted(TurnCompletedNotification {
                thread_id: "t".to_string(),
                turn,
            }),
            &mut reported,
            ctx(),
        );
    }

    fn reasoning(id: &str, summary: &str) -> ThreadItem {
        ThreadItem::Reasoning {
            id: id.to_string(),
            summary: vec![summary.to_string()],
            content: Vec::new(),
        }
    }

    fn agent_text(transcript: &Transcript, key: &str) -> Option<String> {
        match &transcript.entry(key)?.item()?.item {
            ThreadItem::AgentMessage { text, .. } => Some(text.clone()),
            _ => None,
        }
    }

    #[test]
    fn lag_resync_repairs_a_message_truncated_by_dropped_deltas() {
        // A thread started in the tab: no history was ever loaded.
        let mut transcript = Transcript::new();
        apply(&mut transcript, started(agent("m", "")));
        apply(&mut transcript, delta("m", "The answer is"));
        // Deltas and `item/completed` were dropped; the turn ends.
        turn_completed(&mut transcript, "turn-1", TurnStatus::Completed);
        assert_eq!(
            agent_text(&transcript, "m").as_deref(),
            Some("The answer is")
        );
        assert!(transcript.history.attach("t"));
        transcript.resync(
            vec![("turn-1".to_string(), agent("m", "The answer is 42."))],
            ctx(),
        );
        assert_eq!(
            agent_text(&transcript, "m").as_deref(),
            Some("The answer is 42.")
        );
        assert_eq!(row_ids(&transcript), vec!["m#0", "turn-turn-1#0"]);
    }

    /// A legacy `thread/read` of `transcript`'s live items: same turns and
    /// text, synthetic ids.
    fn legacy_snapshot() -> Vec<(String, ThreadItem)> {
        vec![
            ("t1".to_string(), user("item-0", None, "first question")),
            ("t1".to_string(), reasoning("item-1", "**Thinking**")),
            ("t1".to_string(), agent("item-2", "first answer")),
            ("t2".to_string(), user("item-3", None, "second question")),
            ("t2".to_string(), agent("item-4", "second answer")),
        ]
    }

    fn live_legacy_session() -> Transcript {
        let mut transcript = Transcript::new();
        transcript.history.mode = Some(codex_app_server_protocol::ThreadHistoryMode::Legacy);
        apply(
            &mut transcript,
            completed_in("t1", user("u-1", None, "first question")),
        );
        apply(
            &mut transcript,
            completed_in("t1", reasoning("r-1", "**Thinking**")),
        );
        apply(
            &mut transcript,
            completed_in("t1", agent("a-1", "first answer")),
        );
        turn_completed(&mut transcript, "t1", TurnStatus::Completed);
        apply(
            &mut transcript,
            completed_in("t2", user("u-2", None, "second question")),
        );
        apply(
            &mut transcript,
            completed_in("t2", agent("a-2", "second answer")),
        );
        transcript
    }

    #[test]
    fn legacy_resync_does_not_duplicate_live_items() {
        let mut transcript = live_legacy_session();
        let keys = notice_keys(&transcript);
        assert_eq!(
            transcript.live_turns(),
            HashSet::from(["t1".to_string(), "t2".to_string()])
        );
        transcript.adopt_snapshot(&legacy_snapshot());
        assert!(transcript.live_turns().is_empty());
        transcript.resync(legacy_snapshot(), ctx());
        // Same rows, still under the live ids later events use.
        assert_eq!(notice_keys(&transcript), keys);
        assert_eq!(transcript.model.len(), transcript.rows());
    }

    #[test]
    fn legacy_reload_after_a_restart_does_not_repeat_live_items() {
        let mut transcript = live_legacy_session();
        let keys = notice_keys(&transcript);
        let mut turns = HashMap::new();
        turns.insert("t1".to_string(), {
            let mut turn = turn(TurnStatus::Completed, None);
            turn.id = "t1".to_string();
            turn
        });
        transcript.adopt_snapshot(&legacy_snapshot());
        let added = transcript.prepend_history(legacy_snapshot(), &turns, ctx());
        assert_eq!(added, 0);
        assert_eq!(notice_keys(&transcript), keys);
    }

    #[test]
    fn legacy_refetch_after_a_trim_restores_live_items_once() {
        let mut transcript = live_legacy_session();
        transcript.push_notice(NoticeKind::Info, "", "after the first answer", ctx());
        // Move the notice right after `a-1` (before the turn end).
        let notice = transcript.remove_entry(transcript.entries.len() - 1);
        let at = transcript.index["a-1"] + 1;
        transcript.insert_entries(at, vec![notice], ctx());
        assert_eq!(
            notice_keys(&transcript),
            vec!["u-1", "r-1", "a-1", "notice-1", "turn-t1", "u-2", "a-2"]
        );
        // Keep the last row: `t2`'s question is trimmed with `t1`.
        let boundary = transcript.trim_front(1, usize::MAX).flatten();
        assert_eq!(
            boundary,
            Some(TrimBoundary {
                id: "u-2".to_string(),
                turn_id: Some("t2".to_string()),
            })
        );
        assert_eq!(notice_keys(&transcript), vec!["a-2"]);
        // The legacy read cannot find `u-2` and resumes after turn `t2`.
        let turns = vec![
            Turn {
                items: legacy_snapshot()[..3]
                    .iter()
                    .map(|(_, item)| item.clone())
                    .collect(),
                ..{
                    let mut turn = turn(TurnStatus::Completed, None);
                    turn.id = "t1".to_string();
                    turn
                }
            },
            Turn {
                items: legacy_snapshot()[3..]
                    .iter()
                    .map(|(_, item)| item.clone())
                    .collect(),
                ..{
                    let mut turn = turn(TurnStatus::Completed, None);
                    turn.id = "t2".to_string();
                    turn
                }
            },
        ];
        let slice = super::super::history::legacy_slice(
            turns,
            super::super::history::LegacyBoundary {
                include_from: Some("u-2"),
                turn_id: Some("t2"),
            },
            &transcript.live_turns(),
        );
        let metadata: HashMap<String, Turn> = slice
            .turns
            .into_iter()
            .map(|turn| (turn.id.clone(), turn))
            .collect();
        transcript.adopt_snapshot(&slice.snapshot);
        transcript.prepend_history(slice.items, &metadata, ctx());
        // Everything comes back once, in order, with the notice in place.
        assert_eq!(
            notice_keys(&transcript),
            vec![
                "item-0", "item-1", "item-2", "notice-1", "turn-t1", "item-3", "a-2"
            ]
        );
        assert!(transcript.stashed.is_empty());
        assert_eq!(transcript.model.len(), transcript.rows());
    }

    #[test]
    fn unsent_messages_stay_visible_but_are_not_pending() {
        let mut transcript = Transcript::new();
        let hi = [crate::session::text_input("hi")];
        transcript.push_local_user_message("c1", &hi, ctx());
        assert!(transcript.mark_echo_unsent("c1", ctx()));
        let block = &transcript.entry("c1").map(|entry| entry.blocks[0].clone());
        assert_eq!(
            block
                .as_ref()
                .map(|block| (block.status, block.detail.as_str())),
            Some((super::super::blocks::Status::Failed, "Not sent"))
        );
        // A new identical message is confirmed by the server; the failed
        // one is not mistaken for it.
        transcript.push_local_user_message("c2", &hi, ctx());
        apply(&mut transcript, completed(user("u2", None, "hi")));
        assert_eq!(notice_keys(&transcript), vec!["c1", "u2"]);
        // Not part of an export.
        let sources = super::super::export::entry_sources(transcript.entries());
        assert_eq!(sources.len(), 1);
        // It can be trimmed (and comes back with the history around it).
        apply(&mut transcript, completed(agent("a", "reply")));
        assert!(transcript.trim_front(1, usize::MAX).is_some());
        assert_eq!(notice_keys(&transcript), vec!["a"]);
        assert_eq!(transcript.stashed.len(), 1);
    }

    #[test]
    fn deleted_queued_messages_lose_their_bubble() {
        let mut transcript = Transcript::new();
        transcript.push_local_user_message("c1", &[crate::session::text_input("later")], ctx());
        assert!(transcript.remove_echo("c1"));
        assert!(transcript.entries().is_empty());
        assert_eq!(transcript.model.len(), 0);
        assert!(!transcript.remove_echo("c1"));
    }

    #[test]
    fn repeated_history_errors_use_one_row() {
        let mut transcript = Transcript::new();
        apply(&mut transcript, completed(agent("a", "text")));
        for attempt in 0..3 {
            transcript.set_notice(
                "history-error",
                NoticeKind::Warning,
                "History",
                &format!("failed {attempt}"),
                ctx(),
            );
        }
        assert_eq!(notice_keys(&transcript), vec!["a", "history-error"]);
        transcript.clear_notice("history-error");
        assert_eq!(notice_keys(&transcript), vec!["a"]);
    }

    #[test]
    fn interrupted_tool_calls_stop_running() -> serde_json::Result<()> {
        let items: Vec<ThreadItem> = [
            serde_json::json!({
                "type": "mcpToolCall", "id": "mcp", "server": "s", "tool": "t",
                "status": "inProgress", "arguments": {}
            }),
            serde_json::json!({
                "type": "dynamicToolCall", "id": "dyn", "tool": "t",
                "arguments": {}, "status": "inProgress"
            }),
            serde_json::json!({
                "type": "fileChange", "id": "patch", "status": "inProgress",
                "changes": [{"path": "/repo/a.rs", "kind": {"type": "add"}, "diff": "x\n"}]
            }),
            serde_json::json!({
                "type": "collabAgentToolCall", "id": "agent", "tool": "wait",
                "status": "inProgress", "senderThreadId": "p",
                "receiverThreadIds": ["c"], "agentsStates": {}
            }),
        ]
        .into_iter()
        .map(serde_json::from_value)
        .collect::<serde_json::Result<_>>()?;
        let mut transcript = Transcript::new();
        for item in items {
            apply(&mut transcript, started(item));
        }
        let running = |transcript: &Transcript| {
            transcript
                .entries()
                .iter()
                .flat_map(|entry| &entry.blocks)
                .filter(|block| block.status == super::super::blocks::Status::Running)
                .count()
        };
        assert!(running(&transcript) >= 4);
        turn_completed(&mut transcript, "turn-1", TurnStatus::Interrupted);
        assert_eq!(running(&transcript), 0);
        let statuses: Vec<String> = transcript
            .entries()
            .iter()
            .filter_map(|entry| {
                Some(match &entry.item()?.item {
                    ThreadItem::McpToolCall { status, .. } => format!("{status:?}"),
                    ThreadItem::DynamicToolCall { status, .. } => format!("{status:?}"),
                    ThreadItem::FileChange { status, .. } => format!("{status:?}"),
                    ThreadItem::CollabAgentToolCall { status, .. } => format!("{status:?}"),
                    _ => return None,
                })
            })
            .collect();
        assert_eq!(
            statuses,
            vec!["Failed", "Failed", "Declined", "Interrupted"]
        );
        Ok(())
    }

    #[test]
    fn large_payloads_are_dropped_or_counted() -> serde_json::Result<()> {
        let image = "A".repeat(4 * 1024 * 1024);
        let generation: ThreadItem = serde_json::from_value(serde_json::json!({
            "type": "imageGeneration", "id": "img", "status": "completed",
            "revisedPrompt": "a cat", "result": image
        }))?;
        let pasted: ThreadItem = serde_json::from_value(serde_json::json!({
            "type": "userMessage", "id": "u", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "url": format!("data:image/png;base64,{image}")}
            ]
        }))?;
        let mut transcript = Transcript::new();
        apply(&mut transcript, completed(generation));
        apply(&mut transcript, completed(pasted));
        assert!(transcript.bytes() < 64 * 1024, "{}", transcript.bytes());
        let kept = transcript.entry("u").and_then(Entry::item).map(|item| {
            serde_json::to_value(&item.item)
                .ok()
                .and_then(|value| value.pointer("/content/1/url").cloned())
        });
        assert_eq!(
            kept,
            Some(Some(serde_json::Value::String(
                "data:image/png;base64,".to_string()
            )))
        );
        // What is kept is counted against the memory caps.
        let tool: ThreadItem = serde_json::from_value(serde_json::json!({
            "type": "dynamicToolCall", "id": "d", "tool": "t", "arguments": {},
            "status": "completed",
            "contentItems": [{"type": "inputText", "text": "x".repeat(100_000)}]
        }))?;
        let before = transcript.bytes();
        apply(&mut transcript, completed(tool));
        assert!(transcript.bytes() - before >= 100_000);
        Ok(())
    }

    #[test]
    fn live_turn_ends_come_back_after_a_trim() {
        let mut transcript = Transcript::new();
        apply(&mut transcript, completed_in("t1", agent("a1", "one")));
        let mut failed = turn(TurnStatus::Failed, Some("quota exceeded"));
        failed.id = "t1".to_string();
        let mut reported = None;
        transcript.apply(
            &ServerNotification::TurnCompleted(TurnCompletedNotification {
                thread_id: "t".to_string(),
                turn: failed,
            }),
            &mut reported,
            ctx(),
        );
        apply(&mut transcript, completed_in("t2", agent("a2", "two")));
        let before = notice_keys(&transcript);
        assert_eq!(before, vec!["a1", "turn-t1", "a2"]);
        transcript.trim_front(1, usize::MAX);
        assert_eq!(notice_keys(&transcript), vec!["a2"]);
        // A history page brings `a1` back; its turn end (with the failure)
        // is rebuilt from what `turn/completed` reported.
        let turns = transcript.history.turns.clone();
        transcript.prepend_history(vec![("t1".to_string(), agent("a1", "one"))], &turns, ctx());
        assert_eq!(notice_keys(&transcript), before);
        let error = match &transcript.entry("turn-t1").map(|entry| &entry.body) {
            Some(Body::TurnEnd(end)) => end.error.clone(),
            _ => None,
        };
        assert_eq!(error.as_deref(), Some("quota exceeded"));
    }

    #[test]
    fn toggling_expands_rows() {
        let mut transcript = Transcript::new();
        let item = ThreadItem::Reasoning {
            id: "r".to_string(),
            summary: vec!["**Plan**\n\nDetails here.".to_string()],
            content: Vec::new(),
        };
        apply(&mut transcript, completed(item));
        assert_eq!(transcript.rows(), 1);
        // Expanded: the body without the repeated headline.
        assert!(transcript.toggle("r#0", ctx()));
        assert_eq!(transcript.rows(), 2);
        assert!(transcript.toggle("r#0", ctx()));
        assert_eq!(transcript.rows(), 1);
        assert!(!transcript.toggle("missing#0", ctx()));
    }
}
