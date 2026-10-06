# codex-gui: a native Slint front end for Codex

Status: implemented on branch `feature/codex-gui` (crate `codex-rs/gui`).
Section 12 records what shipped, measured results, and where the
implementation deviates from this plan. Sections 1–11 are the original plan,
kept for context; corrections are marked inline.

This document is the plan for `codex-gui[.exe]`, a second binary built from
this workspace that opens a native, low-resource, tabbed GUI instead of the
terminal UI. It records what already exists in the codebase that we can
reuse, the architectural decisions, the feature checklist, the risks, and the
phased delivery order.

---

## 1. Goals

1. **Everything Codex CLI can do, with a mouse.** Feature parity with the
   TUI (`codex-rs/tui`): threads, turns, approvals, exec output, patches,
   diffs, MCP, skills, plugins, hooks, review, plan mode, compaction, fork,
   resume, model picker, images, `@` mentions, sub-agents.
2. **Browser-style tabs.** One tab per agent thread, each bound to a folder.
   Many agents run concurrently and independently. No "project" concept.
3. **Inter-agent messaging between tabs.** Agents in separate tabs can send
   each other messages, and the user can forward content between tabs.
4. **Native performance, tiny footprint.** Slint, single process, single
   binary, no IPC, no web view, no open port by default. Works without a GPU
   (Azure Virtual Desktop, VMs) through software rendering.
5. **Scales to very long threads.** Transcript rendering is a sliding window
   over on-disk history. Memory does not grow with thread length.
6. **Settings as a GUI**, not slash commands: a schema-driven settings pane
   plus a one-click AWS Bedrock setup page with profile and model pickers.
7. **Any inference path Codex supports.** OpenAI, ChatGPT login, Amazon
   Bedrock (Mantle and Runtime endpoints), Ollama, LM Studio, custom
   Responses-API providers.
8. **Plain text file viewer** with selectable text. No syntax highlighting,
   no LSP.
9. **Windows and macOS first**, Linux should work because Slint and winit do.

Non-goals (for now): IDE features, terminals, git clients, syntax
highlighting, LSP, mobile, remote server mode as a default.

---

## 2. Survey: does anything already meet all criteria?

No. Checked October 2026.

| Project | Stack | Why it fails the criteria |
|---|---|---|
| OpenAI Codex / ChatGPT desktop app | Electron, closed | Windows Store only on Windows (no AVD), web renderer, closed source. The app-server now exposes `account/bedrock/*` RPCs, so Bedrock may land there, but the other blockers remain. |
| t3code | Web UI + IPC to Codex | Web rendering, client/server with open port, slow startup, "project" model. |
| jk-gan/agent-hub | Rust + GPUI, spawns `codex app-server` child | GPUI has no software renderer (same blocker as Zed on AVD). Cross-process JSON-RPC over stdio. macOS primary. |
| 3h2oto/agentx, CES-Ltd/Lumi | Rust + GPUI, ACP | GPU required; generic ACP client, not Codex-native. |
| wieslawsoltes/CodexGui | .NET + Avalonia, spawns app-server | Not Rust, .NET runtime, child process. Avalonia does have software rendering. Closest in spirit. |
| CodexMonitor, monocode, Codexia/OnlyCode, codex-app-plus, Nimbalyst, desktop-cc-gui | Tauri / Electron + React | Web view rendering, JS single-thread UI, child process to app-server. CodexMonitor inactive since March 2026. |
| Redminote11tech/Codex-Native | Rust + GTK/WebKitGTK shell around official frontend | Web view, Linux only. |
| Codex CLI TUI | ratatui | No mouse text editing. Otherwise the reference feature set. |

Conclusion: write it. But most of the hard parts already exist in this repo.

---

## 3. What already exists in this repo that we build on

Verified at HEAD `80e0b51c9e`. Paths relative to `codex-rs/`.

### 3.1 The TUI already runs the app-server in-process

The TUI does **not** link `codex-core` directly any more. `tui/Cargo.toml`
depends on `codex-app-server-client`, `codex-app-server-protocol` and
`codex-app-server-daemon`. It starts an embedded app-server:

- `app-server-client/src/lib.rs:328` `InProcessAppServerClient` with
  `start`, `request_typed`, `notify`, `next_event`,
  `resolve_server_request`, `reject_server_request`, `shutdown`.
- `app-server/src/in_process.rs:1-38`: "runs the existing `MessageProcessor`
  and outbound routing logic on Tokio tasks, but replaces socket/stdio
  transports with bounded in-memory channels." Requests are typed
  `ClientRequest` values; notifications and server requests arrive typed and
  boxed (`AppServerEvent::ServerNotification(Box<ServerNotification>)`).
  Only responses travel through a JSON-RPC result envelope
  (`serde_json::Value` → typed decode), which is a handful of small
  conversions per turn.
- `tui/src/lib.rs:321` `AppServerTarget { Embedded, LocalDaemon, Remote }`.
  Embedded is the default; the daemon is opt-in.
- `tui/src/app_server_session.rs:323` `AppServerSession` is a typed wrapper:
  `start_thread`, `fork_thread`, `thread_read`, `turn_start`,
  `turn_interrupt`, `turn_steer`.
- `tui/src/app/startup.rs:1238-1325` is the main `select!` loop over UI
  events and `app_server.next_event()`.
- `exec/src/lib.rs:22` uses the same client. `thread-manager-sample/` shows
  the alternative of driving `codex_core::ThreadManager` directly.

**This is the architecture for the GUI.** Same process, same Tokio runtime,
same typed protocol, zero sockets. We get every RPC the official desktop app
uses, maintained upstream, and we never re-implement event mapping,
approvals, config editing, Bedrock setup, thread listing or history
projection.

### 3.2 Multi-thread is native to the app-server

- One `Arc<ThreadManager>` per process (`app-server/src/message_processor.rs:336`).
- One connection subscribes to many threads; notifications carry
  `thread_id`. Per-connection subscription state in
  `app-server/src/thread_state.rs`.
- RPCs: `thread/start|resume|fork|list|read|loaded/list|archive|
  inject_items|queue/*`, `turn/start|steer|interrupt`, `review/start`.
- Notifications: `thread/started`, `turn/started|completed`,
  `item/started|completed`, `item/agentMessage/delta`,
  `item/reasoning/*Delta`, `item/commandExecution/outputDelta`,
  `thread/tokenUsage/updated`, `turn/diff/updated`, `turn/plan/updated`.
- Server requests (approvals): `item/commandExecution/requestApproval`,
  `item/fileChange/requestApproval`, `item/tool/requestUserInput`,
  `mcpServer/elicitation/request`, `item/permissions/requestApproval`.

Tabs map 1:1 to threads over a single in-process connection.

### 3.3 History is already on disk and paginated

- Rollouts: `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`
  (optionally `.jsonl.zst`), JSONL of `RolloutLine` (`history/src/lib.rs:361`).
- `ThreadHistoryMode::Paginated` (`protocol/src/protocol.rs:753`): UI history
  is served from a SQLite projection (`thread-store/src/local/thread_history*.rs`,
  `thread_history_1.sqlite`) via `thread/turns/list` and `thread/items/list`.
  Model context is rebuilt by a reverse scan to the last compaction
  (`thread-store/src/local/model_context.rs:28-36`).
- The TUI loads 5 turns initially and pages 100 items at a time
  (`tui/src/app_server_session/history.rs:30-32`,
  `tui/src/app/history_pagination.rs`), and caps per-thread replay buffers
  (`tui/src/app/thread_event_buffer.rs`: 4 KiB delta coalescing, 256 KiB cap).
- Thread listing: `thread/list` is cursor-paginated, filesystem scan with
  SQLite read-repair (`rollout/src/list.rs`, `MAX_SCAN_FILES=10000`).

So "keep most of the context on disk" is already how Codex works. The GUI's
job is to not duplicate it in UI memory.

### 3.4 Amazon Bedrock is built in

- Crate `aws-auth`: SigV4 signing (`signing.rs`), SDK default credential
  chain incl. profiles and SSO (`config.rs`), `discover_aws_profiles()` and
  `validate_aws_profile()` (`discovery.rs`).
- Crate `model-provider/src/amazon_bedrock/`: `BedrockSigV4AuthProvider`,
  auth source precedence (command bearer → `aws.credential_export` →
  `aws.profile` → Codex-managed keys → `AWS_BEARER_TOKEN_BEDROCK` → env
  access keys → SDK chain), auth refresh via `aws sso login`.
- Two built-in providers (`model-provider-info/src/lib.rs`):
  `amazon-bedrock` (Mantle, `https://bedrock-mantle.{region}.api.aws/openai/v1`,
  SigV4 service `bedrock-mantle`) and `amazon-bedrock-runtime`
  (`https://bedrock-runtime.{region}.amazonaws.com/openai/v1`). Both
  `WireApi::Responses`.
- Model catalog is static (`catalog.rs`, `runtime_catalog.rs`); there is no
  `ListFoundationModels` call yet (`app-server/.../bedrock_setup.rs:127` TODO).
- App-server RPCs: `account/bedrock/discover`, `account/bedrock/setup`
  (`{type:"profile",profile,region}` or `{type:"environment",region}`),
  `account/bedrock/checkGovCloudRequirements`; login variants
  `amazonBedrock{apiKey,region}` and `amazonBedrockAccessKeys`.
- The TUI already has a Bedrock onboarding wizard (`tui/src/onboarding/bedrock.rs`).

Config shape the GUI will write:

```toml
model_provider = "amazon-bedrock"          # or "amazon-bedrock-runtime"
model = "openai.gpt-5.6-luna"

[model_providers.amazon-bedrock.aws]
profile = "my-profile"                     # optional; else SDK chain / env
region  = "us-west-2"                      # optional; else SDK / env
```

### 3.5 Config is typed, layered, schema'd, and editable over RPC

- `config/src/config_toml.rs:166` `ConfigToml` (~120 top-level keys).
- Layer stack with per-key origins (`config/src/loader/`), precedence:
  packaged defaults < MDM < system < enterprise < user < profile < project <
  session flags < managed.
- JSON schema: `core/config.schema.json` (7.9k lines), generated by
  `codex-write-config-schema` (`config/src/schema.rs:279`
  `config_schema_json()`). This is the complete key list.
- RPCs: `config/read` (effective config + origins + layers),
  `config/value/write` (`key_path`, `value`, `merge_strategy`,
  `expected_version`), `config/batchWrite` (`reload_user_config` hot-reloads
  most settings into live threads), `configRequirements/read`,
  `config/mcpServer/reload`. Writes use `toml_edit` and preserve formatting.
- The TUI writes config via `ClientRequest::ConfigValueWrite`
  (`tui/src/app/background_requests.rs:1247`).

A schema-driven settings pane is therefore mostly a renderer over
`config/read` + `config.schema.json` with `config/value/write` on change.

### 3.6 Multi-agent today: one tree per root thread

- v1 `multi_agent` (on by default): `spawn`, `send_input`, `wait`,
  `close_agent`, `resume_agent` (`core/src/tools/handlers/multi_agents/`).
- v2 `multi_agent_v2` (off by default): `spawn_agent`, `send_message`,
  `followup_task`, `wait_agent`, `list_agents`, `interrupt_agent`.
- `ext/agent-message-board`: channels/threads/posts in SQLite, gated behind
  `agent_message_board` + `multi_agent_v2`; membership is one agent tree.
- `Op::InterAgentCommunication` and `RolloutItem::InterAgentCommunication`
  persist agent-to-agent messages.
- **Two independent root threads cannot message each other through agent
  tools today**: `ensure_agent_known` rejects targets outside the caller's
  tree (`core/src/agent/control/api.rs:100-160`). Cross-thread input exists
  only client-side: `turn/start`, `thread/inject_items`, `thread/queue/add`
  accept any thread id.
- `app-server/src/dynamic_tools.rs` + `DynamicToolCallRequest` /
  `Op::DynamicToolResponse` let a client register tools the agent can call
  and the client answers.

Section 5.6 uses the last two facts to deliver cross-tab messaging without
core changes.

### 3.7 Other reusable pieces

- Workspace already has `arboard` (clipboard), `image`, `pulldown-cmark`,
  `diffy`, `similar`, `webbrowser`, `tokio`, `tracing`, `toml_edit`.
- `codex-file-search` + RPC `fuzzyFileSearch/*` for `@` mentions.
- RPCs `fs/readFile`, `fs/writeFile`, `fs/readDirectory`, `fs/watch`,
  `fs/changed` for the file viewer.
- `arg0` crate: multicall dispatch (`codex-linux-sandbox`, `apply_patch`
  sentinels), `.env` loading, PATH shim dir, Tokio runtime bootstrap
  (`arg0/src/lib.rs:60,219`).
- Release pipelines: `rust-release.yml` (macOS DMG, signing, notarization),
  `rust-release-windows.yml` (MSVC, trusted signing), bundles per binary.
- Toolchain 1.95.0, edition 2024, `[profile.release]` thin LTO.

---

## 4. Why Slint, and how

### 4.1 Slint facts (1.18.1, September 2026)

- Rust-native declarative UI, compiled `.slint` files via `slint-build` in
  `build.rs`. MSRV 1.92 (we are on 1.95).
- Backends/renderers: `winit` (default on all platforms since 1.16) with
  `renderer-femtovg` (OpenGL), `renderer-femtovg-wgpu` (Metal / Vulkan /
  D3D12 via wgpu), `renderer-skia` (heavy), `renderer-software` (pure CPU,
  no deps), experimental `renderer-vello`. Selection order skia → femtovg →
  software, overridable with `SLINT_BACKEND=winit-software`.
- Software renderer limits: no rotation/scaling, no drop shadows, limited
  border-radius clipping. (Correction: with `std` it shapes text with parley,
  so it is not limited to western scripts; that limit applies only to
  embedded bitmap fonts.)
- `TextEdit`/`TextInput`: multi-line, mouse selection, cut/copy/paste,
  undo/redo (1.14+), `read-only` keeps selection enabled, `set-selection-offsets`,
  `select-all`, cursor callbacks. AccessKit exposes text and selection.
- `StyledText` + `@markdown()` (1.16+): inline bold, italic, strike, inline
  code, links with `link-clicked`, lists, `<u>`, `<font color>`. **Not**
  supported: headings, fenced code blocks, tables, block quotes, images,
  rules. Not selectable. (Correction: `@markdown` is compile-time; runtime
  text uses `slint::StyledText::from_markdown`, which errors on unsupported
  constructs and stray `<tags>`.)
- `ListView` instantiates only visible rows (1.16+), handles varying item
  heights; custom `Model` implementations give lazy `row_data`.
- Threading: event loop on the main thread; background threads call
  `slint::invoke_from_event_loop` / `Weak::upgrade_in_event_loop`;
  `slint::Timer` for ticks.
- License: `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0`.
  The royalty-free license covers desktop applications at no cost and has no
  source obligation, but **attribution is mandatory** (the `AboutSlint` widget
  in an About dialog reachable from the top-level menu, or a public badge).
  See risk R6 and section 12.

### 4.2 Renderer strategy

| Situation | Renderer | Notes |
|---|---|---|
| macOS | femtovg-wgpu (Metal) or femtovg (OpenGL) | Always has a GPU path. |
| Windows with GPU | femtovg-wgpu (D3D12) | |
| Windows without GPU (AVD) | femtovg-wgpu on D3D12 **WARP** software adapter, fall back to `renderer-software` | WARP gives full-quality text with no GPU. Spike item S3 verifies wgpu picks WARP. |
| Linux | femtovg (GL) or software | |

Design the UI inside the software renderer's constraints (flat surfaces, no
shadows, no rotated elements) so the two paths look identical. Skia is
excluded: C++ build, large binary. Expose "Force software rendering" in
settings (sets `SLINT_BACKEND` before init).

### 4.3 Threading model

```
main OS thread                      Tokio multi-thread runtime (N workers)
┌──────────────────────┐            ┌─────────────────────────────────────┐
│ Slint event loop     │  UiBatch   │ bridge task: drains                 │
│ winit window         │◄───────────│   client.next_event()               │
│ Slint models/props   │ invoke_    │   coalesces deltas per ~16 ms       │
│ callbacks → commands │ from_event │   pushes UiBatch to UI thread       │
│                      │ _loop      │                                     │
│                      │  GuiCommand│ request tasks: client.request_typed │
│                      │───────────►│ in-process app-server + codex-core  │
│                      │ mpsc       │ (all threads/agents live here)      │
└──────────────────────┘            └─────────────────────────────────────┘
```

- UI thread never blocks on I/O. Slint callbacks only push a `GuiCommand`
  onto an unbounded `tokio::sync::mpsc`.
- The bridge task batches everything destined for the UI into one
  `UiBatch` per frame budget, then a single `invoke_from_event_loop`. Deltas
  for the same item are concatenated before crossing. This keeps the
  in-process event channel drained (the runtime drops notifications under
  saturation and reports `Lagged`).
- `arg0` today spawns a `codex-main` thread with the Tokio runtime and
  blocks the real main thread joining it (`arg0/src/lib.rs:219`). Slint
  needs the real main thread (AppKit). Add
  `arg0_dispatch_or_else_keep_main_thread(...)`: same `.env` + PATH-shim +
  multicall handling, but it builds the runtime on background threads,
  returns `(Arg0DispatchPaths, tokio::runtime::Handle)` and leaves the main
  thread to the caller. Small, additive change to `arg0`.

---

## 5. Design

### 5.1 Crate and binary

- New crate `codex-rs/gui/` → package `codex-gui`, `[[bin]] name = "codex-gui"`.
- Dependencies: `codex-app-server-client`, `codex-app-server-protocol`,
  `codex-app-server-daemon` (optional daemon/remote), `codex-arg0`,
  `codex-config` + `codex-app-server-client::legacy_core::config` for
  startup `Config`, `codex-protocol`, `slint`, `slint-build`,
  `pulldown-cmark`, `arboard`, `tokio`, `tracing`, `serde_json`.
- `main.rs`: `arg0_dispatch_or_else_keep_main_thread` → `codex_gui::run_main`.
  Keep `codex` (the multitool) free of Slint; optionally add `codex gui`
  later as a thin exec of the sibling `codex-gui` binary.
- Windows: `#![cfg_attr(windows, windows_subsystem = "windows")]` so no
  console flashes. The `apply_patch` shim invokes `codex_self_exe` with a
  sentinel; piped stdio still works without a console. Verify in spike S5.
- Bazel: add `BUILD.bazel` with `codex_rust_crate`; `slint-build` runs as a
  cargo build script, so confirm `rules_rs` build-script support, then
  `just bazel-lock-update`. Cargo remains the source of truth.
- Release: add `codex-gui` to the macOS `primary` bundle (DMG) and the
  Windows `primary` bundle in the release workflows. Later.

Source layout:

```
codex-rs/gui/
  Cargo.toml  build.rs  BUILD.bazel
  ui/
    app.slint            # window, tab strip, sidebar, panes
    transcript.slint     # virtualized transcript list + block components
    composer.slint       # TextEdit composer, attachments, popups
    approvals.slint      # approval cards, user-input prompts
    settings/*.slint     # schema-driven settings, bedrock, mcp, etc.
    theme.slint          # tokens; flat, software-renderer-safe
  src/
    main.rs              # arg0 keep-main-thread, run_main
    app.rs               # AppState: tabs, thread registry, UI model wiring
    bridge.rs            # tokio <-> slint batching
    session.rs           # typed RPC wrapper (port of tui app_server_session)
    transcript/          # block store, markdown -> blocks, sliding window
    composer.rs          # mentions, slash equivalents, image paste
    settings/            # schema loader, form model, bedrock pane
    files.rs             # file viewer tabs
    xtab.rs              # cross-tab messaging (dynamic tool + queue)
    keymap.rs            # shortcuts
```

### 5.2 Window layout

```
┌ codex-gui ──────────────────────────────────────────────────────────────┐
│ [≡] │ ● repo-a: refactor auth │ ○ repo-b: tests │ ⚠ repo-a #2 │ + │ ⚙  │  tab strip
├─────┼───────────────────────────────────────────────────────────┬───────┤
│ S   │ transcript (virtualized, sliding window)                  │ info  │
│ i   │  ┌ user ───────────────────────────────┐                  │ cwd   │
│ d   │  │ ...                                 │                  │ model │
│ e   │  └─────────────────────────────────────┘                  │ tokens│
│ b   │  ┌ agent ──────────────────────────────┐                  │ diff  │
│ a   │  │ StyledText paragraph                │                  │ agents│
│ r   │  │ ┌ code (read-only TextEdit) ──────┐ │                  │ plan  │
│     │  │ └─────────────────────────────────┘ │                  │ queue │
│ thr │  │ ▸ exec `cargo test` ✓ 2.1s  [output]│                  │       │
│ eads│  └─────────────────────────────────────┘                  │       │
│ by  │  ┌ approval: run `rm -rf target`? [Allow] [Deny] [Always] │       │
│ cwd ├───────────────────────────────────────────────────────────┤       │
│     │ composer (TextEdit): mouse select/cut/paste, @, /, images │       │
│     │ [model ▾] [effort ▾] [approvals ▾] [plan] [send]          │       │
└─────┴───────────────────────────────────────────────────────────┴───────┘
```

- **Tab strip**: tab = thread (or a file view, or settings). Status glyph:
  idle / streaming / awaiting approval / error. Unread dot on background
  activity. Ctrl/Cmd+T new, Ctrl/Cmd+W close, Ctrl+Tab cycle, drag to
  reorder, middle-click close, right-click: rename, fork, archive, move to
  new window (later), copy thread id.
- **New tab** picks a folder (last used, recent list, or picker) then
  `thread/start` with that `cwd`. No projects; the sidebar groups threads by
  `cwd` from `thread/list`.
- **Sidebar**: thread list (paginated `thread/list`, search, archived
  toggle), collapsed to icons by default.
- **Info pane** (collapsible): model/effort/provider, token usage
  (`thread/tokenUsage/updated`), turn diff (`turn/diff/updated`), plan
  (`turn/plan/updated`), sub-agents, queued messages (`thread/queue/*`).
- **Composer**: Slint `TextEdit` gives the mouse editing the TUI lacks.
  Enter sends, Shift+Enter newline (configurable). `@` opens fuzzy file
  search (`fuzzyFileSearch/session*`), `/` opens the command palette
  (section 6), image paste via `arboard` → `UserInput::LocalImage`.
  Esc interrupts (`turn/interrupt`). Typing while streaming steers
  (`turn/steer`) or queues (`thread/queue/add`), user's choice in settings.

### 5.3 Transcript: blocks, sliding window, streaming

**Block model.** Each `ThreadItem` becomes one or more `Block`s:

| Item | Blocks |
|---|---|
| UserMessage | one `Paragraph` per paragraph + attachments |
| AgentMessage | markdown → `Paragraph(StyledText markup)`, `Heading`, `CodeBlock(lang, text)`, `Table(rows)`, `Quote`, `Rule`, `ListBlock` |
| Reasoning | collapsed `Reasoning` block (summary), expandable |
| CommandExecution | `Exec{cmd, status, duration}` + collapsible `Output` (ANSI stripped, tail-limited) |
| FileChange | `Patch{files, +/-}` + per-file diff blocks |
| McpToolCall / DynamicToolCall / WebSearch / ImageGeneration | compact tool cards |
| CollabAgentToolCall / SubAgentActivity | agent cards linking to the sub-agent view |
| Plan / ContextCompaction / errors | status blocks |

Markdown → blocks uses `pulldown-cmark` (already a dependency). Paragraph
inline content is passed as CommonMark source to `StyledText` via
`@markdown`, since its subset (bold/italic/code/links/lists) is exactly the
inline subset. Code blocks render as read-only monospace `TextEdit` so they
are selectable and copyable. Tables render as a grid of `Text`.

**Virtualization.** The transcript is a `ListView` over a custom
`slint::Model` (`TranscriptModel`) whose `row_data(i)` materializes a block
from the per-tab `BlockStore` on demand. Only visible rows exist as Slint
elements. `BlockStore` keeps:

- hot blocks: fully parsed, near the viewport and the live tail;
- cold blocks: compact `(item_id, kind, byte_len, est_height)`; content is
  re-fetched via `thread/items/list` by item id range when scrolled to;
- a monotone height estimate cache so the scrollbar is stable.

Cap hot blocks per tab (default 2,000) and total hot bytes (default 8 MiB
per tab, 64 MiB app-wide). Background tabs drop everything but the tail
window and the metadata index.

**Loading.** Open/resume a thread in `Paginated` history mode: load the
last 5 turns (`thread/turns/list` + `thread/items/list`, same limits the
TUI uses), render immediately, fetch older pages when the user scrolls
within two viewports of the top. A 10k-item thread opens in constant time.

**Streaming.** Port the TUI's stable-region / mutable-tail idea
(`tui/src/streaming/controller.rs`): completed paragraphs are committed as
immutable blocks; only the trailing unfinished paragraph is re-parsed and
re-rendered on each batch. Deltas are coalesced per frame in the bridge.
Exec output deltas append to a bounded ring (tail-limited, "show full
output" opens a file-view tab backed by the full item).

**Copy.** Per-block copy button, per-message "copy as markdown", "copy
turn", "export thread as markdown" (TUI `markdown_copy` has the logic to
port). Code blocks and the composer support native drag-select. Cross-block
drag-select across the whole transcript is Phase 3 (risk R1).

### 5.4 Approvals and server requests

Server requests arrive as `AppServerEvent::ServerRequest`. Each becomes an
inline card at the transcript tail and a badge on the tab: command approval
(allow / deny / always for session / edit command), file change approval
(with diff), permissions request, user-input questions (`requestUserInput`),
MCP elicitation. Resolution calls `resolve_server_request`. Approval mode
switcher in the composer bar maps to `TurnSettings`/`ThreadSettings`
(approval policy + sandbox), the same thing `/permissions` does.

### 5.5 Settings pane

Three pages:

1. **Common** (curated): model + reasoning effort (`model/list`), provider,
   approvals + sandbox, notifications, theme, renderer, send/steer behavior,
   keymap. Each control is bound to a config key path.
2. **All settings** (schema-driven): walk `config.schema.json` (embedded via
   `include_str!` or generated at build time with `config_schema_json()`),
   group by top-level table, render by JSON type: bool → `Switch`, enum →
   `ComboBox`, string → `LineEdit`, number → `SpinBox`, arrays and tables →
   expandable sub-forms or a raw TOML `TextEdit`. Show the origin layer from
   `config/read` next to each value ("set by project .codex/config.toml",
   "managed by MDM", read-only when the layer is not writable).
3. **Raw TOML**: `TextEdit` over the user `config.toml` with validation on
   save through `config/batchWrite`.

Writes go through `config/value/write` with `expected_version` for
conflict detection and `reload_user_config` so live threads pick changes up.
Dedicated sub-pages for MCP servers (`config/mcpServer/reload`), profiles,
skills, plugins, hooks, memories, keymap, and the AWS page below.

### 5.6 AWS Bedrock page

1. "Use Amazon Bedrock" toggle; endpoint choice: Mantle (`amazon-bedrock`)
   or Runtime (`amazon-bedrock-runtime`).
2. Profile picker populated by `account/bedrock/discover` (which uses
   `discover_aws_profiles`), plus "environment credentials" option and
   `AWS_BEARER_TOKEN_BEDROCK` / access keys entry (stored via the existing
   `amazonBedrock*` login variants).
3. Region picker (Mantle region list from `model-provider/src/amazon_bedrock/mantle.rs`,
   runtime regions, GovCloud check via `account/bedrock/checkGovCloudRequirements`).
4. "Validate" runs `validate_aws_profile`; "Apply" calls
   `account/bedrock/setup`, which writes `model_provider` and
   `model_providers.<id>.aws.{profile,region}`.
5. Model picker from `model/list` (static catalog today). Stretch: live
   discovery by signing a `GET {base_url}/models` with `AwsAuthContext::sign`
   and merging the result; this also closes the `bedrock_setup.rs:127` TODO.
6. Ollama / LM Studio page reuses `ensure_oss_provider_ready` and the
   `fetch_models` / `pull_model_stream` clients for local models.

### 5.7 Inter-agent messaging across tabs

Phase 1 (no core changes):

- **User-forwarded**: select a block or message, "Send to tab…". Delivered
  with `thread/queue/add` (durable, dispatched when the target is idle) or
  `turn/start` if the user wants it now. Prefixed with provenance
  (`[from tab "repo-a: refactor auth", thread <id>]`).
- **Agent-initiated**: the GUI registers dynamic tools on every thread it
  starts (`ThreadStartParams.dynamic_tools`): `list_open_threads()`,
  `send_message_to_thread(target, message, wait_for_reply: bool)`,
  `read_thread_mailbox()`. The agent's `DynamicToolCallRequest` is answered
  by the GUI: it resolves the target tab, enqueues the message in the
  target via `thread/queue/add` (or `thread/inject_items` for non-turn
  context), records it in a GUI-side mailbox (SQLite in `CODEX_HOME`, same
  shape as `InterAgentCommunication`), and returns the delivery receipt.
  Replies route back through the same tool. User can inspect every
  cross-tab message in the info pane and disable the feature per tab.

Phase 3 (optional, upstream-friendly): extend `agent-message-board`
membership to a user-scoped "workspace" so root threads share a board, and
let `send_message` resolve any loaded root thread. Keep this separate and
small so it can be upstreamed.

### 5.8 File viewer

Tab kind `File`: `fs/readFile` into a read-only monospace `TextEdit`
(selectable, copyable, wrap toggle), `fs/watch` for live reload, Ctrl+F
find, line numbers in the gutter. Opened from file citations in agent
output, from patch cards, and from "open file" in the sidebar. Large files
are chunked (first 2 MiB, "load more"). Diff viewer: unified diff from
`turn/diff/updated` / `item/fileChange` rendered as colored blocks using
`similar` for intra-line highlights.

### 5.9 Sub-agents

`CollabAgent*` and `SubAgentActivity` items render as agent cards. The info
pane lists children (`agent-graph-store` edges via `thread/read`). Clicking
opens the child thread in a nested view or a new tab (`thread/resume` on the
child id), the GUI analogue of `/agents` and `/subagents`.

### 5.10 Daemon and remote (opt-in only)

Default `AppServerTarget::Embedded`, no socket, no port. Settings offer
"Connect to local daemon" (`connect_local_daemon`, shares threads with the
TUI and `codex agents`) and "Connect to remote app-server" (`ws://`, UDS)
for driving a machine over a tailnet. Off by default to honor the
single-binary, no-port goal.

---

## 6. Feature parity checklist (TUI slash commands → GUI)

From `tui/src/slash_command.rs`. "RPC" names the app-server method where known.

| TUI | GUI affordance |
|---|---|
| `/model` | Composer model/effort dropdown (`model/list`) |
| `/permissions` | Composer permissions picker: Read only, Auto, Auto-review (`approvalsReviewer: auto_review`, shown when the `guardian_approval` feature is on and requirements allow it), Full access (asks for confirmation first) |
| `/approve` | `/approve` palette entry: confirms and approves one retry of the newest auto-review denial (`thread/approveGuardianDeniedAction`); run again for older denials |
| `/setup-default-sandbox` | Settings › Approvals; elevated setup wizard |
| `/new`, `/clear` | New tab (Ctrl/Cmd+T), folder picker |
| `/resume` | Sidebar thread list (`thread/list`), search |
| `/fork`, `/side`, `/btw` | Tab menu › Fork; "ephemeral fork" opens a scratch tab |
| `/rename`, `/archive`, `/delete` | Tab context menu (`thread/archive`, delete RPC) |
| `/compact`, `/recap` | Tab menu › Compact / Recap |
| `/review` | Review button (`review/start`) with target picker |
| `/plan` | Plan toggle in composer; plan pane (`turn/plan/updated`) |
| `/diff` | Info pane diff; full diff tab |
| `/mention` | `@` popup (`fuzzyFileSearch/*`) |
| `/status`, `/usage`, `/debug-config` | Info pane; Settings › Diagnostics (config layers, requirements) |
| `/mcp`, `/apps`, `/plugins`, `/skills`, `/hooks`, `/memories` | Settings sub-pages; MCP OAuth login buttons |
| `/init` | Tab menu › Create AGENTS.md |
| `/goal` | Goal field in info pane |
| `/agents`, `/subagents` | Sub-agent cards and info pane list |
| `/copy`, `/export` | Copy buttons; Export thread as Markdown |
| `/cd`, `/pwd` | Tab's cwd shown in tab + info pane; change via folder picker |
| `/worktree` | Tab menu › Continue in worktree (`codex-worktree`) |
| `/keymap`, `/vim` | Settings › Keymap (no vim mode in GUI composer initially) |
| `/experimental` | Settings › Features (feature flags) |
| `/theme` | Settings › Appearance (light/dark/system; no syntax themes) |
| `/import` | Settings › Import from Claude Code |
| `/logout`, login | Settings › Account (ChatGPT, API key, Bedrock) |
| `/ps`, `/stop` | Info pane › Background terminals |
| `/voice` | Deferred; `realtime-webrtc` exists but out of scope for MVP |
| `/daemon` | Settings › Connection (section 5.10) |
| `/feedback`, `/warnings` | Help menu; warnings banner |
| `/quit` | Close window; confirm when turns are running |
| `/ide`, `/app`, `/tui`, `/raw`, `/title`, `/statusline`, `/pets`, `/daybreak` | Not applicable or cosmetic; skip |

Also: images (paste/attach), ANSI exec output, notifications (desktop
toast on approval needed / turn complete), queued messages, token usage,
context-window gauge, onboarding/login flow.

---

## 7. Performance and memory budgets

Targets, measured on a 2-vCPU, 4 GiB, no-GPU Windows VM and on an Apple
Silicon Mac. Checked in CI where feasible (startup, idle RSS) and manually
otherwise.

| Metric | Target |
|---|---|
| Cold start to first frame | < 300 ms (window and tab strip appear before the embedded app-server finishes init) |
| Cold start to usable (thread list + composer) | < 1 s |
| Idle RSS, 1 tab | < 80 MiB |
| Idle RSS, 10 tabs | < 200 MiB (core keeps model context per thread; UI adds < 5 MiB per background tab) |
| Opening a 10,000-item thread | < 1 s, independent of length |
| Streaming frame rate | 60 fps with GPU, ≥ 30 fps software at 1080p; ≤ 1 UI invoke per frame |
| UI thread blocking | never > 8 ms per callback (all I/O on Tokio) |
| Binary size (release, stripped) | < 60 MiB on Windows and macOS |

Mechanisms: paginated history, hot/cold block store, delta coalescing,
stable/tail streaming, background-tab eviction, `ListView` virtualization,
no web view, thin LTO, software-renderer-safe visuals.

---

## 8. Risks and mitigations

| # | Risk | Mitigation |
|---|---|---|
| R1 | Slint `Text`/`StyledText` are not selectable; cross-block drag selection over the transcript is not provided by the toolkit. | Phase 1: selectable code blocks (read-only `TextEdit`), per-block/message/turn copy, "copy as markdown". Phase 3: custom selection layer (hit-test block rows, compute offsets via `TextInput` metrics, render highlight rectangles). If still unsatisfying, offer "open message in editor view" which is one big read-only `TextEdit`. |
| R2 | Software renderer shapes western scripts only. | Default to femtovg-wgpu, which on Windows can run on D3D12 WARP (software) with full text shaping. Spike S3 confirms. Pure software renderer stays as last resort. |
| R3 | `StyledText`/`@markdown` is new (1.16) and lacks headings, code blocks, tables. | We render those as separate blocks; `StyledText` only gets inline-level markup. Spike S2 validates the Rust API for building styled text at runtime. |
| R4 | In-process event channel drops notifications under saturation (`Lagged`). | Dedicated drain task, per-frame batching, bounded exec output; on `Lagged` re-sync via `thread/read` + `thread/items/list`. |
| R5 | `arg0` owns the main thread. | Additive `arg0_dispatch_or_else_keep_main_thread`. TUI/exec untouched. |
| R6 | Licensing. Codex is Apache-2.0; Slint is GPLv3 OR Royalty-free-2.0 OR commercial. | Use the royalty-free license for the `codex-gui` binary (desktop app, no royalty, attribution optional). Confirm the RF 2.0 text permits an open-source non-GPL distribution; fallback is licensing only the `codex-gui` crate as GPL-3.0 while the rest of the workspace stays Apache-2.0. Decide before Phase 1. |
| R7 | Bazel + `slint-build` build script. | Verify `rules_rs` build-script support in S5; fall back to the `slint!` macro (no build.rs) if needed. |
| R8 | Protocol churn upstream. | We use the same app-server protocol as the official app and TUI; bump with the workspace. Keep GUI logic behind a `session.rs` wrapper like `AppServerSession`. |
| R9 | Windows `windows_subsystem = "windows"` and the `apply_patch` shim. | S5 verifies piped stdio works; otherwise point `codex_self_exe` at a sibling `codex.exe` when present. |
| R10 | Memory of core per thread is outside GUI control. | Expose compaction and token gauge; document that core context, not UI, dominates per-tab memory. |

---

## 9. Phased plan

### Phase 0: Spike (1–2 weeks). Decision gate.

- S1 `codex-rs/gui` crate skeleton, Slint window, tab strip mock, builds
  on macOS and Windows with cargo. Measure cold start and RSS.
- S2 Start the embedded app-server with `InProcessAppServerClient` from a
  GUI (needs the `arg0` keep-main-thread entry), `thread/start`,
  `turn/start`, stream `item/agentMessage/delta` into `StyledText` via the
  bridge. Validate the Rust API for `styled-text`.
- S3 Renderers on a GPU-less Windows VM (AVD or Hyper-V without GPU):
  femtovg-wgpu (expect WARP), pure software. Record fps, text quality, RSS.
- S4 Virtualized `ListView` with a lazy `Model` of 50k synthetic blocks of
  varying height; scroll smoothness, memory, scrollbar stability.
- S5 Build plumbing: Bazel `BUILD.bazel` with `slint-build`,
  `windows_subsystem`, `apply_patch` shim from the GUI exe.
- Gate: S2 streams at ≥ 30 fps on software rendering with < 100 MiB RSS,
  S4 scrolls smoothly, S6 (license) resolved.

### Phase 1: MVP (usable daily)

Tabs (new/close/switch, folder-bound), thread list + resume (paginated),
composer `TextEdit` with send/steer/interrupt, transcript blocks for
user/agent/reasoning/exec/patch/tool items, approvals (command, file
change, permissions, user input), model/effort picker, approvals/sandbox
switcher, token usage, login (ChatGPT, API key), **Bedrock page**,
Settings › Common, light/dark theme, desktop notifications, Windows +
macOS release artifacts.

### Phase 2: Parity

Everything in section 6 not yet done: schema-driven settings, MCP / skills /
plugins / hooks / memories pages, review, plan mode, fork, compact/recap,
images, `@` mentions, file viewer tabs, diff tab, sub-agent cards, queued
messages, export/copy-as-markdown, keymap editor, import from Claude Code,
feature flags, diagnostics (config layers), onboarding flow, Linux build.

### Phase 3: Killer features and polish

Cross-tab agent messaging via dynamic tools (5.7), optional message-board
extension upstream, cross-block text selection (R1), daemon/remote connect
(5.10), worktree flow, multi-window, live Bedrock model discovery, voice if
`realtime-webrtc` is cheap to wire, accessibility pass (AccessKit), perf CI.

---

## 10. Open questions

1. Royalty-free 2.0 license text vs Apache-2.0 distribution (R6).
2. ~~Does `ThreadStartParams.dynamic_tools` exist?~~ Confirmed:
   `v2/thread.rs:148` takes `Vec<DynamicToolSpec>` (from
   `codex_protocol::dynamic_tools`). S2 still needs to verify the call /
   response round trip through `DynamicToolCallRequest`.
3. Enter-to-send vs Ctrl+Enter default. Proposal: Enter sends, Shift+Enter
   newline, configurable.
4. Should `codex-gui` ship inside the `codex` npm package, or as a separate
   download? Proposal: separate asset in the same GitHub release first.
5. Vim mode in the composer: skip for MVP; revisit if requested.

---

## 11. References

- Slint: https://slint.dev, docs https://docs.slint.dev, license
  https://slint.dev/pricing.html, StyledText
  https://docs.slint.dev/latest/docs/slint/reference/elements/styledtext/,
  backends https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backends_and_renderers/
- Codex Bedrock help: https://help.openai.com/en/articles/20001253
- Alternatives surveyed: jk-gan/agent-hub, wieslawsoltes/CodexGui,
  3h2oto/agentx, CES-Ltd/Lumi, CodexMonitor, monocode, PKQ1688/OnlyCode,
  fraternity-z/codex-app-plus, Nimbalyst/nimbalyst, Redminote11tech/Codex-Native.

---

## 12. Implementation status (October 2026)

The crate `codex-rs/gui` builds the `codex-gui` binary. Developer notes live
in `codex-rs/gui/README.md`; the user guide is `docs/gui.md`.

### 12.1 What shipped

| Plan item | Status |
|---|---|
| §4.3 threading: `arg0_dispatch_or_else_keep_main_thread`, Tokio on workers, one batched UI post per 16 ms frame with delta coalescing | Done (`arg0`, `gui/src/backend.rs`) |
| §5.1 crate, binary, Windows subsystem, Bazel target, release workflow | Done; `.github/workflows/rust-release-gui.yml` builds unsigned artifacts that include the helpers the runtime finds next to `codex-gui` (`codex-code-mode-host`; on Windows `codex-windows-sandbox-setup` and `codex-command-runner`); the window icon is embedded from Rust (`include_bytes!`), so the Bazel compile needs no build-script paths; `MODULE.bazel.lock` still needs `just bazel-lock-update` |
| §5.2 tabs, sidebar, info pane, composer, keyboard shortcuts (user keymap) | Done: tabs shrink, then scroll, with a tab list; drag to reorder; sidebar and info pane become drawers in narrow windows; Ctrl+Tab cycles tabs (⌃Tab on macOS) |
| §5.3 transcript blocks, streaming (stable region + tail), paged history, hot/cold trimming, copy/export | Done |
| §5.4 approvals (command, file, permissions, questions, MCP elicitation, sub-agent routing) | Done |
| §5.5 settings (common, schema-driven, raw TOML, account, MCP, skills, plugins, hooks, features, appearance, diagnostics) | Done |
| §5.6 Bedrock page (Mantle and Runtime, profiles, env, API key, access keys, GovCloud check, restart, model picker) and local models (Ollama, LM Studio) | Done; live `ListFoundationModels` discovery and LM Studio downloads not done |
| §5.7 cross-tab messaging (dynamic tools `codex_gui.*`, queue delivery, wait-for-reply, mailbox, user forwarding) | Done; an agent's message is delivered only after the user allows it on a card in the target tab ("allow for this session" per tab pair; always asked when the target has broader permissions, and again after three hops); mailbox is JSONL (`$CODEX_HOME/gui/mailbox.jsonl`), not SQLite |
| §5.8 file viewer and diff viewer tabs | Done |
| §5.9 sub-agent cards and info pane list | Done |
| §5.10 daemon / remote app-server (opt-in) | Done (`--remote unix://`, `ws://`, `wss://`; Settings › Connection). A connection that cannot be used never blocks startup: the window reports it and offers the embedded server |
| Quit and shutdown | File › Quit, closing the window, and on macOS ⌘Q / Dock › Quit (`applicationShouldTerminate:` added to winit's delegate) confirm when turns run and shut the app-server down; logout shuts down without asking |
| §6 slash-command parity | Done except `/voice` (deferred) and cosmetic TUI-only commands |
| R1 cross-block selection | Fallback shipped: "View as text" opens a selectable text tab; per-block selection everywhere else |
| Multi-window | Not done (Slint globals are per window; would need per-window controllers) |

### 12.2 Measured performance (macOS, Apple Silicon, Retina, release build)

| Metric | Budget (§7) | Measured |
|---|---|---|
| Cold start to first frame | < 300 ms | 74 ms from a terminal; 192 ms from Finder or the Dock (includes about 46 ms reading the login shell's `PATH`) |
| Cold start to usable (server ready) | < 1 s | 190 ms from a terminal; 322 ms from Finder or the Dock |
| Memory, 1 idle tab | < 80 MiB | 125 MiB footprint with the software renderer; 346 MiB footprint (207 MiB RSS) with OpenGL, mostly GPU driver allocations |
| Memory per extra tab | < 5 MiB | about 2 MiB (RSS 207 MiB with 1 tab, 224 MiB with 10) |
| Binary size | < 60 MiB | 220 MiB stripped (codex-core with V8; the shipped `codex` CLI is 241 MiB) |

Renderer choice was measured, not assumed (8.7 s session with a streaming
reply, then 10 s idle):

| Renderer | CPU, session | CPU, idle 10 s | Peak footprint |
|---|---|---|---|
| FemtoVG OpenGL (default) | 1.2 s | 0.04 s | 355 MiB |
| Software | 6.7 s | 0.08 s | 142 MiB |
| FemtoVG WGPU | 7.4 s | 0.83 s | 509 MiB |

The default is OpenGL, falling back to software when OpenGL is unavailable:
Slint probes for OpenGL on Windows; on Linux the app checks for the GL
libraries first and, if OpenGL still fails when the window opens, restarts
itself with `--renderer software` (Slint's renderer cannot change in-process).
Settings › Appearance offers "Software" for the smallest memory footprint and
"GPU" (WGPU, which can use Windows' WARP adapter when `SLINT_WGPU_CPU` is set;
the app sets it for that choice). Run `codex-rs/gui/dev/measure.sh` to
reproduce.

### 12.3 Corrections to this plan found during implementation

- §3.1: the TUI now connects to a shared local daemon by default
  (`daemon_auto_start` is stable). The GUI embeds its own server, so the TUI
  and GUI are separate app-server processes on one `CODEX_HOME`; opening the
  same thread in both hits "already has an active writer". `--remote unix://`
  makes the GUI use the daemon instead.
- §4.1 / §4.2: Slint selects femtovg-wgpu, then software, when no renderer is
  named; GL FemtoVG is reachable only by name. WARP is used only with
  `SLINT_WGPU_CPU`. Attribution is mandatory (Help › About shows `AboutSlint`).
- §5.6: `account/bedrock/setup` only configures Mantle; Runtime is configured
  with `config/batchWrite`. Provider changes require restarting the embedded
  app-server (the model catalog is fixed at startup); the GUI does this.
- §5.7: dynamic tools are registered with `defer_loading: false` under the
  `codex_gui` namespace; `thread/queue/*` requires `experimental_api` and the
  state DB, both of which the GUI provides.
- §5.2: "Ctrl/Cmd+Tab" cannot work on macOS (the system app switcher takes
  ⌘Tab); the default there is ⌃Tab, as in Safari and Chrome.
- Platform gaps Slint and winit leave to the app (`gui/src/platform.rs`):
  macOS `terminate:` (⌘Q from Slint's default app menu, Dock › Quit) exits
  without `CloseRequested`; Finder/Dock launches get launchd's minimal
  `PATH` (the app reads the login shell's, without `~/.zshrc`, whose
  completion setup can block on privacy prompts in an app); the Windows GUI
  subsystem has no console for `--help`.

### 12.4 Remaining work

- Run `just bazel-lock-update` (needs Bazel) so Bazel CI picks up Slint's crates.
- Sign and notarize the macOS `Codex.app` (including the helpers in
  `Contents/MacOS`); add a Windows icon resource.
- Windows installer: register an AppUserModelID so toasts show as Codex (they
  show as Windows PowerShell today).
- Linux: ship `bwrap` with the GUI (the bundled-bwrap lookup only knows the
  CLI's package layout; the GUI uses the system `bubblewrap`).
- Measure on a GPU-less Windows VM (spike S3): software vs WGPU on WARP.
- Live Bedrock model discovery; LM Studio downloads; multi-window; voice.
