# codex-gui: a native Slint front end for Codex

Status: implemented in `codex-rs/gui` on `feature/codex-gui`, committed as
`8233eb8804`, and published on `allquixotic/codex` as
[`codex-gui-v0.1.1`](https://github.com/allquixotic/codex/releases/tag/codex-gui-v0.1.1)
(Windows x64 zip). macOS release builds are deferred by request.

This document retains the goals, architectural decisions, feature coverage,
risks and delivery plan, followed by the October 2026 implementation report.
Planned behavior is distinguished from what shipped. For operating instructions
see [the user guide](docs/gui.md); for development see
[the crate README](codex-rs/gui/README.md).

## 1. Goals

1. **Codex with a mouse:** bring the TUI's threads, tools, approvals, reviews,
   plans, configuration and agent workflows to a native desktop interface.
2. **Browser-style tabs:** one independent agent thread per tab, each bound to
   a folder (or the user’s home in folderless mode), with concurrent work and no separate project concept.
3. **Cross-tab messaging:** let agents coordinate across tabs with user consent,
   and let users forward replies between threads.
4. **Native performance:** Slint UI, embedded app-server, no web view or open
   port by default, and a software rendering path for GPU-less machines.
   The delivered package includes the runtime's helper executables; the
   app-server itself remains in-process.
5. **Long-thread scalability:** page history from disk and bound the UI's
   resident transcript instead of retaining the whole conversation.
6. **Graphical settings:** common controls, schema-driven configuration and an
   AWS Bedrock setup page.
7. **Provider coverage:** OpenAI/ChatGPT, Bedrock Mantle and Runtime, Ollama,
   LM Studio and custom Responses-API providers.
8. **Selectable plain-text files:** file and diff viewers without IDE features.
9. **Platform coverage:** Windows and macOS first, with Linux support.

Non-goals: an IDE, terminal emulator, Git client, syntax highlighting, LSP,
mobile support or remote-server mode by default. Some parity and polish work
remains (§12.6); startup meets the original targets, but memory and binary size
do not.

## 2. Survey: does anything already meet all criteria?

The October 2026 survey found no existing application meeting all requirements.

| Candidate | Main mismatch |
|---|---|
| Official Codex / ChatGPT desktop app | Closed application with web rendering and Windows distribution constraints. |
| t3code | Web UI, client/server transport and a project-oriented model. |
| jk-gan/agent-hub; agentx; Lumi | Rust/GPUI, but GPU-dependent and using a child app-server or generic ACP. |
| wieslawsoltes/CodexGui | Closest in spirit, including software rendering, but .NET/Avalonia with a child app-server. |
| CodexMonitor, monocode, Codexia/OnlyCode, codex-app-plus, Nimbalyst, desktop-cc-gui | Tauri/Electron and a web UI. |
| Codex-Native | GTK/WebKitGTK wrapper, Linux-only. |
| Codex CLI TUI | Reference feature set, but without graphical mouse editing. |

Decision: build a native front end while reusing Codex's existing runtime.
This is the recorded survey rationale, not an ongoing assessment of those projects.

## 3. What already exists in this repo that we build on

The initial code survey used HEAD `80e0b51c9e`. Paths below are relative to
`codex-rs/` unless stated otherwise.

### 3.1 The TUI already runs the app-server in-process

The reusable foundation is `InProcessAppServerClient`: it runs the existing
app-server on Tokio tasks with bounded in-memory channels and typed requests,
notifications and server requests. The GUI uses that path rather than linking
directly to core or implementing another protocol adapter.

The original plan described embedded mode as the TUI default. By the recorded
implementation, the TUI normally used a shared local daemon; the GUI still
embeds its own server. Connecting the GUI to that daemon is optional. Separate
servers cannot both hold the active writer for the same thread.

### 3.2 Multi-thread is native to the app-server

One connection can subscribe to many threads. Thread identifiers route
streaming messages, tool events, approvals, plans, diffs and usage updates to
the correct tab. Start, resume, fork, queue, steer, interrupt and review all
reuse the existing protocol.

### 3.3 History is already on disk and paginated

Rollouts persist under `$CODEX_HOME/sessions`; a SQLite projection serves
paginated thread history. The GUI initially loads a small recent window and
requests older items on demand. It bounds its own display data; core's model
context remains a separate memory cost.

### 3.4 Amazon Bedrock is built in

Codex already provides SigV4, the AWS credential chain, profile discovery,
SSO support, Bedrock login/setup RPCs and two Responses-API providers:
`amazon-bedrock` (Mantle) and `amazon-bedrock-runtime`.

The GUI reuses these facilities. The catalog is static, and setup RPC support
is asymmetric: Mantle uses Bedrock setup; Runtime needs configuration writes.
Provider changes require an embedded-server restart to rebuild the model catalog.

### 3.5 Config is typed, layered, schema'd, and editable over RPC

The configuration schema, effective values, origins, managed requirements and
versioned writes already exist. Settings can therefore render existing data
rather than maintain another configuration model. Writes must respect layer
precedence, policy restrictions and concurrent edits.

### 3.6 Multi-agent today: one tree per root thread

Codex's agent tools and message board coordinate agents within one root tree;
they do not connect unrelated root threads. Client-registered dynamic tools
and thread queues supply the missing cross-tab route without changing core.
The optional extension of the upstream message board was not implemented.

### 3.7 Other reusable pieces

Reuse workspace clipboard, image, Markdown, diff, file-search, filesystem,
logging and async libraries. `arg0` supplies helper dispatch and environment
setup. Existing release machinery supplies platform conventions, while the
GUI has its own workflow. The implementation uses Rust 1.95/edition 2024 and
Slint 1.18.1.

## 4. Why Slint, and how

### 4.1 Slint facts (1.18.1, September 2026)

Slint supplies compiled declarative UI, native text editing, virtualized lists,
accessibility integration and both GPU and CPU renderers. Flat surfaces and
simple geometry keep the UI compatible with software rendering; Skia was
excluded for build and size costs.

Two constraints shape the design:

- `StyledText` handles inline formatting but is not selectable and does not
  implement full Markdown. Block parsing and selectable text views remain
  application responsibilities. Runtime text uses `StyledText::from_markdown`;
  `@markdown` is compile-time only.
- Slint's event loop must own the main OS thread, especially on macOS.

The original claim that software rendering only shapes western scripts was
incorrect for desktop builds with `std`; that restriction concerns embedded
bitmap fonts. Font fallback and missing symbols still need platform testing.

Slint dependencies use Royalty-free 2.0 exceptions in `deny.toml`. Attribution
is mandatory and is supplied by `AboutSlint` in Help › About.

### 4.2 Renderer strategy

| Choice | Delivered behavior |
|---|---|
| Auto | Explicitly selects FemtoVG OpenGL, chosen for measured CPU efficiency; Slint can fall back when the OpenGL probe fails. |
| Software | Pure CPU rendering, with the lowest measured memory footprint. |
| GPU | FemtoVG WGPU on Metal, Vulkan or D3D12; the app enables CPU adapters so Windows WARP is eligible. |

CLI options override saved preferences; `SLINT_BACKEND` can override Slint's
selection. Renderer changes require restarting the process. Linux additionally
relaunches with software rendering after a window-show failure. Windows lacks
that late-failure recovery; explicit software mode is the workaround.
GPU-less Windows software-versus-WARP measurements remain outstanding.

### 4.3 Threading model

The main thread owns Slint and all UI state. Tokio runs the embedded server,
I/O and requests. A dedicated event pump drains server events, merges text
deltas and normally delivers one UI batch per 16 ms frame. Requests cannot
block event draining; dropped notifications trigger a state resync.

An additive `arg0_dispatch_or_else_keep_main_thread` entry preserves helper
dispatch while leaving the main thread to Slint. Process environment changes
run before worker threads start. Async results use stable tab identities and
request generations so stale responses cannot modify a different tab.

## 5. Design

### 5.1 Crate and binary

`codex-rs/gui` builds the `codex-gui` binary and its library. Rust owns runtime,
state and protocol integration; `ui/` contains Slint components. Backend and
session wrappers concentrate protocol access. The existing `codex` binary
remains independent of Slint; a `codex gui` launcher was not added.

Windows uses the GUI subsystem and retains patch/sandbox helper dispatch.
Cargo is the verified build path. A Bazel target exists, but dependency-lock
regeneration and build verification remain open. Distribution uses a separate
GUI workflow and packages the helpers required by core (§12.3).

### 5.2 Window layout

The window combines a tab strip, thread sidebar, central transcript and
composer, and an optional information pane. Narrow windows use drawers.
Tabs cover threads, files/diffs, Settings and the New Tab page.

A new thread begins with a folder and trust check. Tabs show activity and
unread state, support dragging and overflow navigation, and expose lifecycle
actions through menus. The sidebar groups saved threads by folder and provides
search, paging and archive management.

The composer supports mouse editing, attachments, file and skill mentions,
a slash palette, model/effort/permission choices and Plan mode. Enter sends by
default, Shift+Enter inserts a newline, and busy-thread input either steers or
queues according to the user's preference.

### 5.3 Transcript: blocks, sliding window, streaming

Markdown is parsed into paragraphs, headings, lists, quotes, code, tables and
rules. Reasoning, commands, file changes, tools, sub-agents, plans and notices
use dedicated cards. Only visible rows are instantiated.

Completed content forms a stable region while a small mutable tail streams.
Long code and output are chunked or display-limited to avoid relaying out one
growing block. Older rows are trimmed under per-tab limits and fetched again
when needed; background tabs retain a smaller tail.

**Change from the plan:** the compact cold-block index, global byte cap and
stable height cache were not built. The scrollbar reflects loaded content.
Copy actions, export and selectable text tabs shipped; cross-block selection
and “Copy turn” did not.

### 5.4 Approvals and server requests

Command, file-change and permission approvals, agent questions and supported
MCP elicitations share a queued panel above the composer. Decisions return
through the server protocol. Sub-agent requests route to an open ancestor or
another available thread tab.

Cards protect against accidental keystrokes, retain request ownership across
tab changes, and clear stale requests after server restarts. The protocol has
no “edit command” approval decision; declining with guidance is the substitute.

### 5.5 Settings pane

Settings combines curated Common controls, schema-driven All settings, a raw
TOML editor and specialist pages. Controls show origins and managed locks;
versioned writes detect conflicts and reload settings where supported.

Provider changes need a server restart. The raw editor checks TOML syntax,
with semantic errors surfaced after reload. GUI-only preferences live in
`$CODEX_HOME/gui.json`. A dedicated profiles page was not implemented.

### 5.6 AWS Bedrock page

The Providers page selects Mantle or Runtime, discovers AWS profiles, accepts
other supported credentials, chooses a region, validates local profiles and
applies configuration. GovCloud checks are advisory. Models come from the
server's static catalog; live discovery is deferred.

The same page detects Ollama and LM Studio, lists local models and selects a
provider/model. Ollama downloads have progress and cancellation; LM Studio
downloads and automatic loading are absent.

### 5.7 Inter-agent messaging across tabs

GUI-created threads can register dynamic tools to list tabs, send messages
with an optional reply wait, and read their mailbox. Delivery goes through the
target's durable queue and starts a turn with that target's permissions.

Agent messages require receiving-tab consent, except for sender/target pairs
the user allowed for the session. Permission escalation and long message
chains still require confirmation. Provenance, loop limits and wait-cycle
detection constrain coordination. Users can also edit and forward a whole
reply through “Send to tab…”.

The delivered mailbox is a bounded JSONL log, not the proposed SQLite store.
No upstream agent-message-board extension or non-turn context injection was
needed. Detailed behavior and limits are in §12.2.6.

### 5.8 File viewer

Read-only file tabs provide selection, line numbers, find, go to line, wrapping,
reload and chunked loading. File links and patch cards open the appropriate
view. Unified diff tabs provide file collapse, change counts, line numbers
and word-level highlights. Neither viewer adds syntax highlighting.

Remote file operations go through the server. Local external-open actions
confirm before launching a program; remote files cannot be revealed or opened
by the local OS.

### 5.9 Sub-agents

Transcript cards and the information pane show child agents, status and recent
messages. Selecting an agent opens its thread in another tab. A nested agent
view was not implemented.

### 5.10 Daemon and remote (opt-in only)

Embedded mode remains the default. The GUI can connect to a local Unix-socket
daemon or a WebSocket app-server. It does not start or manage a daemon.

Connection settings apply on the next launch or recovery retry. WebSocket
workspaces use server-side paths and search, and inline image attachments.
Authentication tokens come from named environment variables and are refused
over non-loopback plaintext WebSocket connections.

## 6. Feature parity checklist (TUI slash commands → GUI)

This is a capability map, not a promise that every TUI command appears in the
GUI palette. Menus, Settings and the information pane supply some equivalents.

| TUI feature | Delivered GUI equivalent and important limits |
|---|---|
| `/model` | Composer model and supported-effort pickers; Common sets defaults for new threads. |
| `/permissions`, `/approvals` | Permission presets, including policy-gated Auto-review; Full access asks for confirmation. |
| `/approve` | Confirm one retry of an observed auto-review denial; only recent in-memory denials are available. |
| `/setup-default-sandbox` | Settings › Windows sandbox, including elevated setup and standard fallback; no palette entry. |
| `/new`, `/clear` | Immediately create a thread in the current folder; `/new <name>` also names it. |
| `/resume` | Searchable, paginated sidebar with archived threads. |
| `/fork`, `/side`, `/btw` | Whole-thread forks and ephemeral side chats; no “fork from here”. |
| `/rename`, `/archive`, `/delete` | Rename/archive through tab and sidebar actions; Delete exists only in the sidebar. |
| `/compact`, `/recap` | Compaction and a bounded recap generated by a temporary read-only thread. |
| `/review` | Target picker for working changes, branch, commit or custom instructions. |
| `/plan` | Composer toggle, proposed-plan content and progress in the info pane. |
| `/diff` | Turn-diff tab and Changes summary; this is the server's turn diff, not a complete working-tree Git diff. |
| `/mention` | `@` file search, local or server-side according to the connection. |
| `/status`, `/usage`, `/debug-config` | Context, usage limits and configuration diagnostics; no usage-reset action. |
| `/mcp` | MCP status, add/remove/enable and OAuth; no dedicated edit form or full tool browser. |
| `/apps`, `/plugins`, `/skills`, `/hooks`, `/memories` | Settings pages; `/apps` aliases Plugins, not a connector page. Authoring and memory-content viewing are absent. |
| `/init` | Sends the bundled AGENTS.md creation prompt. |
| `/goal` | Info-pane goal controls; no palette entry or token-budget editing. |
| `/agents`, `/subagents` | Cards and info-pane list opening child threads; no palette entries. |
| `/copy`, `/export` | Last reply, block/message copy, full-thread Markdown export and View as Text; no Copy turn. |
| `/cd`, `/pwd`, `/cwd` | Folder shown in the info pane; an existing thread's folder cannot be changed, and these commands are absent. |
| `/worktree` | Fork into a managed checkout; embedded mode only, after a first turn, while idle, with the feature enabled. |
| `/keymap`, `/vim` | Settings › Keyboard edits 18 global actions; composer remapping and vim mode are absent. |
| `/experimental`, `/features` | Managed-policy-aware feature switches. |
| `/theme` | System/light/dark appearance, text size and renderer preference. |
| `/import` | Settings imports from Claude Code and Cursor; no palette entry. |
| `/login`, `/logout` | Account page for ChatGPT browser/device login, API keys and sign-out; other providers use Providers. |
| `/ps`, `/stop` | Background-terminal list and stop controls; `/clean` alias is absent. |
| `/voice` | Deferred. |
| `/daemon` | Settings › Connection or `--remote`; connect-only, no palette entry. |
| `/feedback`, `/warnings` | Help/Settings feedback, warning banner and diagnostics; no palette entries. |
| `/quit`, `/exit` | Quit with confirmation and interruption of running turns; macOS system Quit also follows this path. |
| `/ide`, `/app`, `/tui`, `/raw`, `/title`, `/statusline`, `/pets`, `/daybreak` | Not implemented: outside the chosen GUI scope or cosmetic. |
| `/rollout`, `/test-approval`, `/debug-m-drop`, `/debug-m-update` | Debug-only TUI commands omitted; GUI automation covers request injection. |

Also shipped: image paste/attach, platform-dependent file drops, `$` skill
mentions, confirmed `!` shell commands, queue management, desktop notifications
and context usage. Command output strips ANSI escapes instead of rendering
terminal colors. Images appear as composer attachments but not inline in the
transcript. Further limitations are recorded in §12.2 and §12.6.

## 7. Performance and memory budgets

The priorities are consistently low memory usage and immediate interaction.
Transcript paging, virtualized rows, bounded model-search batches, cancellable
background work and coalesced streaming must keep resident UI data bounded over
long sessions. Repeated searches, opening/closing tabs and long streaming turns
must not cause memory to grow indefinitely. Input, scrolling and menus should
remain responsive; avoid decorative animations that delay interaction.

Startup is already sufficient. Preserve the current behavior, but do not repeat
startup benchmarks or measure all performance targets for every build. Run focused
memory or latency measurements only for a suspected regression or a change that
can affect them. Historical measurements and original aspirational budgets are
retained in §12.4; exceeding an old binary-size or startup target does not by itself
justify another build. No routine performance CI gate is required.

Use targeted development checks while implementing. Run a full release build
only once the entire requested feature set is complete and relevant checks pass.
Release builds are slow and consume substantial disk space; do not use them as
an edit/check loop. Skip macOS release builds and signing/notarization for now.
After uploading completed release binaries to GitHub, verify the published assets
and then remove local build artifacts (Cargo target directories and staging
outputs). Preserve source changes and any files needed independently of a build.
[`publish-release.sh`](codex-rs/gui/packaging/publish-release.sh) uploads completed
packages to an existing release, compares GitHub’s SHA-256 digest for every asset,
then runs `cargo clean` and removes only the exact published local packages. A
missing or mismatched digest leaves all local artifacts in place.

## 8. Risks and mitigations

| Risk | Resolution or remaining exposure |
|---|---|
| R1: Non-selectable styled transcript | Copy and selectable text views shipped; cross-block selection and Copy turn remain open. |
| R2: Software-rendering quality and GPU-less Windows | Desktop software text shaping works; OpenGL is the measured default, with software/WARP options. Windows glyphs and late fallback still need fixes. |
| R3: Incomplete Markdown support | Parse blocks separately and use styled text only for supported inline formatting. |
| R4: Dropped events under load | Dedicated event draining, frame batching and thread/info-pane resync after `Lagged`. |
| R5: Main-thread ownership | Additive arg0 entry leaves the real main thread to Slint. |
| R6: Slint licensing | Royalty-free dependency exceptions and required About attribution are in place. |
| R7: Bazel build scripts/dependencies | Target exists; lock regeneration and verification remain open. |
| R8: Upstream protocol changes | Workspace protocol types and backend/session wrappers keep integration changes visible at compile time. |
| R9: Windows GUI subsystem and helper dispatch | Patch/helper dispatch works; redirected CLI output and executable resources remain incomplete. |
| R10: Core memory outside UI control | Expose compaction and context usage; distinguish bounded UI history from core's retained state. |

## 9. Phased plan

### Phase 0: Spike (1–2 weeks). Decision gate.

Validate the native shell (S1), embedded streaming (S2), GPU-less software/WARP
rendering (S3), large virtualized history (S4), build/helper integration (S5)
and licensing (S6). The intended gate included usable software streaming,
smooth scrolling and a small footprint.

These spikes were folded into implementation. Core architecture, streaming,
virtualization and licensing were established, but the memory gate was missed,
Bazel is unverified and the GPU-less renderer comparison lacks measurements.

### Phase 1: MVP (usable daily)

Tabs, folder trust, history/resume, composer, transcript, approvals, model and
permission controls, login, Bedrock, common settings, themes and notifications.
Implemented; Windows was released first. macOS release builds are deferred.

### Phase 2: Parity

Broader settings, MCP/skills/plugins/hooks/memories, review, plans, forks,
recaps, attachments, file/diff viewers, sub-agents, queues, exports, shortcuts,
imports and diagnostics. Broad coverage shipped, with the explicit gaps in §6.

### Phase 3: Killer features and polish

Cross-tab messaging, remote/daemon connections and worktree continuation
shipped. Cross-block selection, multi-window, live Bedrock discovery, voice,
custom-widget accessibility remains open. Performance checks are focused on regressions (§7). The optional
upstream message-board extension was not pursued.

## 10. Open questions

1. **Slint licensing:** resolved through Royalty-free 2.0 and mandatory
   attribution in Help › About.
2. **Dynamic tool support:** confirmed end to end; the GUI answers its
   `codex_gui` tools through app-server tool-call requests.
3. **Send keys:** Enter sends and Shift+Enter adds a newline by default;
   configurable in Appearance.
4. **Distribution:** separate download assets on a fork release, initially an
   unsigned Windows zip; no npm integration or installer.
5. **Vim composer:** deferred until requested.

Outstanding implementation and product decisions are consolidated in §12.6.

## 11. References

- [Slint](https://slint.dev), [documentation](https://docs.slint.dev),
  [licensing](https://slint.dev/pricing.html),
  [StyledText](https://docs.slint.dev/latest/docs/slint/reference/elements/styledtext/)
  and [renderers](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backends_and_renderers/).
- [Codex Bedrock help](https://help.openai.com/en/articles/20001253).
- Alternatives and tradeoffs: §2.
- [GUI source](codex-rs/gui), [developer README](codex-rs/gui/README.md),
  [user guide](docs/gui.md) and
  [release workflow](.github/workflows/rust-release-gui.yml).

## 12. Implementation status (October 2026)

This records the implementation and verification around commit `163153140f`;
it is not a claim that every planned feature or budget passed. The Windows x64
build is published and in use. The following inventory focuses on capabilities,
important constraints and departures from the original design.

### 12.1 What was asked for and what shipped

| Requested outcome | Recorded result |
|---|---|
| Native, folder-bound, concurrent agent tabs | Shipped with embedded app-server, paged history and mouse-editable composer. |
| Broad TUI parity and graphical settings | Shipped across the main workflows; §6 identifies partial or absent equivalents. |
| Cross-tab agent coordination and user forwarding | Shipped through dynamic tools, receiving-tab consent/session allowances and durable queues. |
| Software rendering / AVD use | Available and used; a controlled GPU-less software-versus-WARP benchmark is still missing. |
| Small footprint and fast startup | Startup succeeds; idle memory and binary size exceed the original budgets. |
| Commit, fork and publish | Commit on `feature/codex-gui`, pushed to `allquixotic/codex`; Windows zip attached to `codex-gui-v0.1.0`. |
| Windows and macOS downloads, unsigned, zip first | Windows complete; macOS asset not attached because its release build was stopped. |
| Record implementation and lessons | This section, supported by the source and development/user documentation. |

### 12.2 Feature inventory

#### 12.2.1 Process, app-server and platform integration

The CLI accepts a folder, a resume thread, renderer selection, a connection
target, an optional token-variable name and config overrides. Resume takes
precedence over a folder. The window opens while the server starts; failure
leaves Settings accessible and offers retry or an embedded-server fallback.
Requested work waits for required sign-in and folder trust.

Embedded startup reuses Codex configuration, policy, state storage, tracing,
telemetry and feedback plumbing. The GUI exposes warnings, logs, diagnostics,
notifications and version/About information. Helper dispatch occurs before UI
startup. The app is a separate executable, not a subcommand of `codex`.

Server restarts rebuild configuration and reattach ordinary threads. Threads
without a first message may need to be recreated; side chats end. Disconnects
and dropped events clear or resync connection-specific state. Closing busy
tabs and quitting confirm, interrupt turns and then shut down cleanly.

Preferences preserve unknown keys, recover invalid values individually and
back up malformed JSON. They save theme, renderer, input behavior, shortcuts,
pane visibility, recent folders, connection settings and window size. Open
tabs, drafts, position and maximized state are not restored after app exit.

Platform integration includes macOS system Quit handling and login-shell PATH
recovery, Windows console/message-box diagnostics, and Linux font and clipboard
handling. Remote/daemon mode has reduced local telemetry and feedback capture.
Connection and renderer changes generally require restarting. See §12.5 for
platform pitfalls and §12.6 for release gaps.

Development hooks include `CODEX_GUI_AUTOMATION`, `CODEX_GUI_PERF`, a mock
Responses server and `gui/dev/measure.sh`. They support scripted UI tours,
snapshots, failure injection and startup/memory measurement.

#### 12.2.2 Window, tabs, sidebar and New Tab page

Tabs support drag reordering, overflow scrolling/list selection, middle-click
close, unread/activity indicators and lifecycle menus. The OS window title is
always “Codex”, independent of the selected tab or thread name. The sidebar provides folder grouping, search, archived history,
paging and thread management. Folder headings have a right-aligned “+” to start
a conversation there. Right-clicking a thread or tab offers Rename and Archive.
Search shows title hits first, then semantic history hits from paginated user and
assistant messages. It prefers the latest Luna (currently gpt-6-luna), preserves
the configured provider and Bedrock profile scope, and retries with the default
conversation model if Luna is unavailable. Replacing a query cancels background
work and unloads temporary model sessions. New Tab offers folderless conversations
(using the local user’s home), folders, recent locations, file
opening and thread resume. Remote connections use a server folder browser.

Folder trust is checked before thread creation and can offer restricted use.
There are unresolved edge cases: trust-check errors rely on server defaults,
and the server may auto-trust a writable folder even after “Open restricted”.
Cross-OS folder browsing is constrained by the protocol's client-native
absolute-path type.

Sidebar and info panes become drawers when space is tight. Shared dialogs,
pickers, notifications and warnings support keyboard use, and 18 global actions
can be rebound. System/light/dark themes and text size are configurable.

Limits: one window, no restored tab session, no tab-strip arrow navigation,
and no completed accessibility pass for custom chrome. Tabs do not display
their computed folder tooltip. Folder-collapse state is temporary. Delete is
sidebar-only; archive confirmation differs between sidebar and tab menus.
Windows/Linux have no dedicated quit shortcut.

#### 12.2.3 Threads and the composer

The composer keeps per-tab in-memory drafts, supports file/skill mentions and
a slash palette, and accepts pasted, selected or dropped image attachments.
Supported image formats are PNG, JPEG, GIF and WebP. Remote images are sent
inline within a 32 MiB message budget; Wayland file drops are unavailable.
Refused sends restore the draft. Input while working steers or queues based on
a global preference; Escape interrupts with recovery for turn-start races.

Model, effort, permissions and Plan mode are applied to the current thread
where supported, otherwise on the next turn. Auto-review follows server
capabilities and managed requirements. Full access and shell commands ask for
confirmation. Recent auto-review denials can be retried through `/approve`.

Lifecycle actions include rename, archive, full-thread fork, side chat,
compaction, recap, review, worktree continuation, AGENTS.md creation, export
and View as Text. Review supports working changes, branches, commits and
custom instructions, with Git queries executed where the workspace lives.
Recap uses the last eight answered exchanges available in the loaded transcript
and a bounded, tool-free temporary thread; it is not a whole-history summary.

Limits: no message-history recall, vim mode, rewind/fork-from-here or persistent
drafts. Side chats are ephemeral, omit inherited paginated history in their
view, and cannot be nested, archived or continued in worktrees. Worktree
continuation requires an idle, materialized thread and the embedded server.
Plan and permission controls depend on the connected server's capabilities.

#### 12.2.4 Transcript

The virtualized transcript renders user/agent Markdown, reasoning, commands,
file changes, tools, searches, image-result notices, sub-agent activity, plans,
hooks, errors, recaps and cross-tab messages. Links and file citations open the
appropriate destination. Local echo, tail following and status notices make
streaming and pending work visible.

Stable completed blocks and a mutable streaming tail limit update costs.
Per-tab caps and smaller background tails bound retained display data. History
loads on scroll; failed pages offer explicit retry. Resync handles dropped
events and legacy rollouts whose item IDs differ from live events.

Copy works for blocks, messages and the last completed reply. Quote in reply,
Send to tab, selectable text views and full-thread Markdown export are also
available. Display truncation generally preserves fuller text for copying;
a live command's full output is available only after completion.

Limits: no cross-block selection, Copy turn, transcript search, syntax
highlighting or inline image previews. The planned cold index/global byte cap
was replaced by dropping and refetching rows. Long content is display-limited,
and history loading pauses at the hard cap. Some protocol item types and
Markdown metadata/footnotes are omitted; recaps and non-error notices are not
exported. Approvals appear above the composer rather than inside the transcript.

#### 12.2.5 Approvals, questions and the info pane

The active tab shows its oldest pending request, with later requests queued.
Supported cards include commands, file changes with diffs, permission requests,
agent questions, MCP forms/links and cross-tab consent. Background tabs signal
waiting through status and notifications. Focus rules and a short input guard
prevent typing intended for the composer from approving a new card.

Request ownership follows sub-agent ancestry. Closing a tab cancels its own
requests and reroutes requests belonging to other threads; restart epochs
prevent replies reaching a new server with reused request IDs. Unsupported
verification/form modes are declined, and external ChatGPT token refresh and
attestation requests are rejected. Server-supplied links are limited to HTTP(S).

The info pane brings together thread settings, context/token use, plans, turn
changes, queued input, sub-agents, background terminals, goals, hooks, MCP
status, cross-tab messages and usage limits. Queued input can be reordered,
sent or removed; goals can be set, edited, paused/resumed or cleared.

Limits: no command editing, goal token-budget controls or usage-reset action.
Lists and previews are bounded rather than exhaustively paginated. Approval
diffs are truncated; form inputs are single-line and unmasked. Unreadable
sub-agent ancestry falls back to an available tab. Section-collapse state is
shared across tabs and not persisted.

#### 12.2.6 Cross-tab agent messaging

GUI-started threads may receive three `codex_gui` tools: list open threads,
send to another thread with an optional reply wait, and read received mailbox
entries. Tools treat peer titles and message content as untrusted data and
instruct agents to coordinate only when the user requests it.

Delivery requires a receiving-tab card, unless the user has allowed that
ordered sender/target pair for the session. A more permissive target or a
message chain beyond three hops always asks again. Escalation compares sandbox,
writable roots, network access and approval policy; unknown permissions count
as escalation, but the check does not cover every setting, including the
approval reviewer.

Messages are limited to 8 KiB, five sends per turn and ten pending messages per
target. Wait-cycle checks reject deadlocks. Accepted messages enter the durable
thread queue and carry escaped provenance. A wait returns the target turn's
reply, or a failure/timeout; a restart drops pending waits but preserves queued
messages. Old queue entries age out of the GUI's pending count because it cannot
observe every user removal.

User forwarding works on whole-message text, allows edits and a note, and can
send now or queue without another consent card. Send now follows the target's
busy-input preference. Attachments and arbitrary selections are not forwarded.

Receiving can be disabled per tab. New threads follow the saved default;
threads reopened from history start with receiving off, and side chats always
have it off. Existing external threads can receive but cannot acquire these
tools merely by toggling the switch. Turning receiving off also disables that
tab's tool calls and revokes allowances into it.

The mailbox is `$CODEX_HOME/gui/mailbox.jsonl`, with bounded history, private
Unix permissions and serialized writes. The info pane shows recent previews,
not a complete archive. Session allowances are in memory; per-tab receiving
state and reply waits are not restored after app exit. Mailbox reads return
incoming messages, while sent-message replies use the wait path. No upstream
message-board integration was added.

#### 12.2.7 Settings

Settings provides Common, All settings, raw `config.toml`, Account, Providers,
MCP servers, Skills, Plugins, Hooks, Features, Memories, Import, Appearance,
Keyboard, Connection, Windows sandbox, Diagnostics and Send feedback pages.

Configuration controls show source layers and policy locks, serialize versioned
writes, refresh after other operations alter config, and distinguish saved
changes from changes needing a restart. Diagnostics selects the project folder
whose layers other settings pages use. Raw local saves preserve file behavior
and detect external edits; remote saves use server filesystem RPCs. The raw
editor validates syntax, not the full configuration schema.

Account supports ChatGPT browser/device login, API keys, sign-out and account
usage. MCP supports adding, toggling, removing and OAuth login; skills can be
toggled, plugins installed/uninstalled/enabled, hooks enabled/trusted, and
memories enabled or reset. Import detects selected configuration and sessions
from Claude Code and Cursor. Features respects managed requirements.

Appearance controls theme, size, renderer, notifications and input behavior.
Keyboard edits the supported global actions. Connection can test a daemon or
remote endpoint and save the next-start target. Windows sandbox supports
readiness and setup. Diagnostics exposes origins, requirements, warnings and
logs; feedback can include server-collected logs.

Limits: no profiles page, MCP edit form, skill/hook authoring, marketplace
addition or memory-content viewer. Composer/file-viewer keys are mostly fixed.
Remote provider changes require restarting the server separately; remote
feedback lacks the GUI process's logs. Large catalog lists and import history
are bounded. A saved connection does not replace the current target through
the ordinary Settings restart action.

#### 12.2.8 Model providers, file viewer and diff viewer

Providers covers OpenAI, Bedrock Mantle/Runtime and local models. Bedrock offers
profile/environment discovery, API-key/access-key entry, regions, local profile
validation, application and return to OpenAI. Managed configuration can lock
provider changes. Switching explicitly selects the destination provider so a
lower configuration layer cannot silently retain the old one.

Bedrock limits: static models, a region list constrained by the existing setup
RPCs, profile-only local validation and advisory post-save GovCloud checks.
Other credentials are exercised by the first model request. Finder launches
import PATH, not arbitrary AWS environment variables. Ollama and LM Studio
models can be selected locally; only Ollama downloads are implemented. Remote
and daemon connections do not offer local model detection/downloads.

File tabs support plain-text selection, path actions, line numbers, find,
go to line, wrap, chunked loading and live reload. Missing files retain the
last loaded contents; binary files offer external opening or forced text.
Programs require confirmation before external launch. Text tabs reuse the
viewer for messages/threads and offer Copy all and Save as.

Files load in chunks up to 64 MiB. Find searches only loaded text; decoding is
lossy UTF-8. Wrap removes the gutter and all-match highlights. Remote reads
transfer the whole file because the protocol lacks ranges, and remote paths
cannot be revealed locally. Without a working watch, changes are checked when
the tab is shown. Images have no preview.

Diff tabs handle multi-file unified/Git diffs, renames, copies, binary markers,
per-file collapse, counts, line numbers and bounded word-level highlighting.
They can open surviving files and copy the raw diff. There is no side-by-side
view, find bar or selectable diff rows. Approval diffs stay on the card;
transcript patch cards open individual file diffs.

### 12.3 Platforms, builds and releases

| Target | Recorded verification | Distribution |
|---|---|---|
| Windows x64 MSVC | Cross-built on macOS with `cargo-xwin`; software/OpenGL smoke runs, PE inspection and interactive use. 595/604 unit tests passed; nine Unix-path assumptions remain to fix. | Published Windows zip. |
| macOS Apple Silicon | 614 unit tests, clippy, scripted end-to-end tours and performance measurements. | Bundle tooling exists; release zip not attached. |
| macOS Intel | Cross target / CI matrix configured; not built in this verification. | None. |
| Windows arm64 | CI matrix configured; not built in this verification. | None. |
| Linux x64 | Musl cross-check compiles and passes clippy; GNU release target configured; not runtime-tested. | None. |

License and dependency-ban checks passed. GUI clippy checks were clean on
macOS, Windows and Linux; an existing app-server warning remained. Mock-model
end-to-end runs covered streaming/Markdown, command and patch workflows,
approvals, plans, settings, file search, cross-tab consent/delivery, recap,
themes, narrow layouts and quit behavior. They do not substitute for all
platform-specific runtime checks.

**Packaging.** The Windows zip contains `codex-gui.exe`,
`codex-code-mode-host.exe`, `codex-windows-sandbox-setup.exe` and
`codex-command-runner.exe`. These are the same runtime boundaries as the CLI:
V8 stays in its host, elevated setup stays small, and sandboxed commands use
their dedicated runner. Helpers must remain next to the GUI. macOS/Linux need
the code-mode helper; `bundle-app.sh` puts it inside `Codex.app`.

**Build requirements.** Code-mode helper builds need Codex's own
`rusty-v8-v<version>` release artifacts for the `ptrcomp_sandbox` variant,
verified against the repository manifest. Use the existing setup action or
its `RUSTY_V8_ARCHIVE` / `RUSTY_V8_SRC_BINDING_PATH` overrides. GUI-only builds
do not need V8. Windows cross-builds additionally use
`LIBSQLITE3_FLAGS=SQLITE_DISABLE_INTRINSIC`, the MSVC target and a stamped
`STABLE_GIT_COMMIT`. Windows test executables can be cross-built and copied to
a VM without installing Rust there.

**Release process.** Fork releases use `codex-gui-v*` tags, distinct from the
upstream `rust-v*.*.*` trigger. The GUI workflow builds
Windows x64/arm64 and Linux x64 artifacts, but does not publish a GitHub
release, sign binaries or upload PDBs. The first Windows release was attached
manually. Releases are unsigned by choice for now, with OS unsigned-app prompts;
there is no installer, DMG or universal macOS bundle. Linux currently requires
system `bubblewrap`.

### 12.4 Measured performance

macOS measurements are from an Apple Silicon release build. Footprint and RSS
are different metrics and are labeled separately.

| Metric | Original budget | Measured |
|---|---|---|
| First frame | < 300 ms | 74 ms from terminal; 192 ms from Finder/Dock, including about 46 ms for shell PATH recovery. |
| Server ready | < 1 s | 190 ms from terminal; 322 ms from Finder/Dock. |
| One idle tab | < 80 MiB RSS | Software: 125 MiB footprint. OpenGL: 207 MiB RSS / 346 MiB footprint. |
| Additional tabs | < 5 MiB each | About 2 MiB each; ten-tab RSS 224 MiB, over the separate 200 MiB total target. |
| Stripped binary | < 60 MiB | 220 MiB; reference CLI 241 MiB. V8 is in the separate helper. |

Windows 11 x64 VM measurements used 14 vCPUs and an NVIDIA RTX 5090, in an SSH
session without model traffic. They are not GPU-less AVD measurements.

| Metric | Software | OpenGL |
|---|---|---|
| Window created | 30 ms | 186 ms |
| First frame | 83 ms | 221 ms |
| Server ready | 309 ms | 586 ms |
| Thread started | 546 ms | 864 ms |
| Clean shutdown | 225 ms | 255 ms |

The Windows GUI executable is 304 MiB; the four-executable zip is 137 MiB.
Workspace code, including generated UI and core, dominates size rather than V8.

A macOS renderer comparison used an 8.7-second streaming session followed by
ten seconds idle:

| Renderer | Session CPU time | Idle CPU time | Peak footprint |
|---|---|---|---|
| OpenGL | 1.2 s | 0.04 s | 355 MiB |
| Software | 6.7 s | 0.08 s | 142 MiB |
| WGPU | 7.4 s | 0.83 s | 509 MiB |

These results motivated OpenGL as the default and software as the lower-memory
option. The software first-frame mark is an event-loop approximation. GPU-less
Windows/WARP comparisons and automated budget enforcement remain open.

### 12.5 Learnings

#### Slint

- Renderer choice must be explicit: the toolkit's unnamed default differs
  from the GUI's measured OpenGL choice. WARP needs CPU adapters enabled.
- Virtualization depends on list structure and incremental model updates.
  Full resets rebuild rows and disturb scrolling; estimated row heights also
  make programmatic restoration approximate.
- Bound streaming work. Chunk long code/output and update only the tail to
  avoid quadratic layout cost. Very large text widgets can still block on
  first layout.
- Overlays must own keyboard focus and suppress global shortcuts. Explicit
  focus and geometry avoid conditional-component and first-change surprises.
  Keep layout thresholds in one place.
- Use platform-resolved fonts and drawn icons where fallback is unreliable.
  Wrapped text does not expose enough geometry for the file gutter; tab
  expansion and shared text layout avoid missing glyphs and line drift.
- System theme is known after window creation; OS drops need winit events.
  Unstable integration APIs are why Slint is pinned. Attribution remains a
  release requirement.

#### App-server and core

- Separate embedded/daemon servers compete for a thread's active writer.
  A thread without a first message has no rollout, and ephemeral threads have
  different history/restart semantics.
- Resume does not reconstruct every live turn or setting. Recover those
  explicitly, and carry unsupported setting updates on the next turn.
  Interrupt before unsubscribing; settle every unfinished card when a turn ends.
- Dropped events require resyncing both history and ancillary state. Legacy
  item identifiers need reconciliation, and failed paging must not retry forever.
- Restarts invalidate requests, watches, catalogs, auth flows and pending
  tool replies. Track a server epoch, re-register connection resources and
  discard stale responses.
- Child-agent requests need ancestry routing and a clear owner when tabs close.
  Dynamic tools persist with GUI-created threads, but receiving is deliberately
  off when reopening history. Queues need the experimental API and state DB.
- Provider changes require fresh server state. Mantle and Runtime use different
  setup paths, and returning to OpenAI must write its provider explicitly.
- Configuration versions change after trust, plugin and skill operations too.
  Serialize writes, refresh origins/defaults from the server, and do not blindly
  retry whole-table replacements after conflicts.
- WebSocket workspaces require server-side filesystem, search and Git queries.
  Unix daemons share local files. Cross-OS absolute paths remain a protocol
  limitation; connection target, not readiness, determines locality.
- Two behaviors remain significant: writable thread startup can defeat the
  intent of “Open restricted”, and default non-Bedrock connection retries can
  continue indefinitely until interrupted or disabled.

#### Architecture and concurrency

UI callbacks can re-enter the controller and be deferred; independent Tokio
requests can reach the server out of order. Sequence dependent operations,
identify tabs by stable IDs, and reject stale generations. A remote file-search
update arriving before its start is a known unresolved example.

Keep I/O, clipboard decoding, external launches and expensive searches off the
UI thread. Change process environment only before worker threads start.

#### Windows

The GUI subsystem can attach to a parent console for help/errors, but currently
loses redirected handles. Missing symbol glyphs require explicit font support
or drawn icons. OpenGL failures after the initial probe need a software relaunch
path like Linux's.

The executable lacks its own application manifest and icon resource; a runtime
window icon does not fix Explorer. An installer-registered AppUserModelID is
also needed for correctly attributed toasts. Manifest work must cover DPI,
supported OS, long paths and common controls, including Bazel link flags.

PE inspection found a static CRT and load-time imports supplied by Windows 11;
no VC++ redistributable is required. Graphics libraries load dynamically.
The header alone is not evidence of support for older Windows versions.
Canonical paths, file URLs, command-line splitting and tests must respect
Windows semantics. The headless SSH smoke test worked, including OpenGL, but
that VM had a passed-through GPU.

#### macOS

Keep Slint on the real main thread and handle system Quit separately from window
close. Use Control-Tab for tab cycling; Command-Tab belongs to the OS. Finder
and Dock need bounded login-shell PATH recovery without interactive startup
scripts. Initialize notification identity on the main thread to avoid the
library's unsafe first-use lookup on a worker.

#### Linux

Resolve monospace fonts through fontconfig, keep clipboard ownership alive,
and account for Wayland's missing file-drop events. OpenGL library/show failures
need software fallback. Distribution currently depends on system bubblewrap.

#### Build, packaging and release

Keep the helper split for V8, elevated setup and sandbox execution. Use verified
Codex V8 artifacts rather than assuming upstream denoland archives cover the
required variant. Cross-builds make Windows verification practical, but each
platform still needs its own clippy and runtime checks.

The GUI workflow trails CLI release conventions: MSVC linker setup, PDB upload
and signing remain open. Neither path currently remaps source paths or enables
Control Flow Guard. Fork-specific tags avoid triggering upstream release flows.

#### Testing

Use the mock Responses server plus UI automation for both real protocol flows
and server requests the mock cannot naturally produce. Separate dependent UI
steps so deferred callbacks finish before snapshots or assertions.

Isolate runs with scratch credentials/config, fake AWS and local-model services,
and distinct ports; inspect persisted results as well as screenshots. Test
remote mode against a local WebSocket server. Review light/dark and narrow
layouts, and reduce toolkit layout failures to small reproductions.

Cross-built Windows unit binaries run without Rust on the VM. The nine recorded
failures came from Unix-specific paths, separators and file URLs; fix fixtures
and rerun rather than calling the Windows suite passing. The expected totals
are 604 Windows tests and 614 macOS tests because some are platform-gated.

#### Recurring bug classes found in review

- Stale async answers, reordered operations and late responses after timeout:
  use stable identities, generations, sequencing and cleanup.
- Retries without progress: stop and offer explicit retry rather than loop.
- Dialog replacement and accidental approval: queue prompts, treat cancellation
  as no action, and protect composer input from newly appearing cards.
- Duplicated parsers, settings or layout rules: share the implementation.
- Remote operations accidentally touching local files: derive behavior from
  connection type consistently.
- Lost preferences or exposed secrets: preserve unknown settings, back up bad
  data, write atomically, restrict sensitive files and keep secrets out of logs.
  Pasted screenshot/log permissions still need attention.
- Cross-tab loops and permission changes: enforce consent, escalation and rate
  rules at delivery as well as at request time.
- Documentation overstating features: distinguish planned, implemented,
  verified and released behavior.

### 12.6 Remaining work

**Release and packaging**

- macOS release builds and signing/notarization are deferred by user request.
- Run `just bazel-lock-update` and verify the GUI target; mirror Windows link
  arguments in Bazel.
- Add a Windows application manifest and executable icon, and an installer
  registering the notification AppUserModelID.
- Align GUI CI's MSVC setup with CLI releases and upload PDBs for crash analysis.
- Add Windows Authenticode when unsigned distribution is no longer desired.
- Bundle `bwrap` in Linux releases rather than require system bubblewrap.

**Windows fixes found in verification**

- Repair missing symbols and platform-specific shortcut labels.
- Preserve redirected output when attaching to the parent console.
- Fix the nine Unix-path-dependent tests and rerun on Windows.
- Recover from late OpenGL failures by relaunching with software rendering.
- Make home-relative path labels use native separators consistently.

**Measurements and reliability**

- Record software versus WGPU/WARP on a GPU-less Windows session host.
- Investigate memory growth or interaction regressions with focused checks;
  no repeated startup measurements or routine per-build benchmarking.
- Serialize dependent backend requests, including remote file-search startup.
- Resolve restricted-folder trust semantics, cross-OS remote path handling and
  restrictive permissions for pasted screenshots/logs.

**Features and polish not done**

- Live Bedrock model discovery and LM Studio downloads.
- Multiple windows, moving tabs between them, restored open tabs and window
  position/state.
- Voice, vim mode, composer history recall and rewind/fork-from-here.
- Cross-block transcript selection, Copy turn and inline image previews.
- MCP edit form, hook/skill authoring, memory-content viewer and profiles page.
- Custom-widget accessibility, tab keyboard navigation and a Windows/Linux
  quit shortcut.

The feature inventory above retains other meaningful limits; minor widget
behavior and implementation recipes belong in the code and developer guide.

## 13. October 6 interaction fixes and operating policy

- Conversation content wraps to the viewport, including long tokens and code;
  the transcript has no horizontal scrolling. The file viewer remains independent.
- Thread and tab right-clicks explicitly open their Rename/Archive menus.
- `/new` creates a thread immediately in the current conversation’s folder;
  `/new <name>` also applies the supplied name to that new thread.
- New Tab’s **New conversation** starts without selecting a project folder,
  using the local user’s home. Remote connections still require a server folder.
- Common and All settings use model and reasoning choices. Context choices use
  bundled model capability limits, retain custom configured values, and store
  numbers as numbers; **Default** removes the override. Offer 1M tokens only if
  the model’s maximum permits it (current bundled GPT entries cap at 872,000).
- History search scans all pages without retaining complete histories, validates
  returned IDs against each batch and keeps title results visible on model failure.
- Follow §7’s memory/interactivity priorities, deferred release-build policy,
  post-upload artifact cleanup and current macOS release deferral.

Verification of these changes: 618 GUI unit tests pass; GUI clippy passes with
warnings denied. A software-rendered mock-provider UI run verified long-token
wrapping at normal and narrow widths, immediate named `/new`, rename/archive,
folder “+”, folderless home creation after the existing trust prompt, constant
window title and history-only search hits. Rejecting gpt-6-luna caused a successful
retry with gpt-6.1-sol on the same provider. Bedrock geographic prefixes and ARNs
are covered by model-selection tests; no live Bedrock model call was made.
Publishing-helper tests verify digest mismatch preserves local artifacts and a
matching digest permits cleanup. These development checks preceded the release
recorded below.

## 14. Windows 0.1.1 release

Published `codex-gui-v0.1.1` from `8233eb8804` on October 6, 2026. The Windows
x64 ZIP contains all four release executables, with x64 PE headers, a static
CRT and the GUI subsystem verified. The finished package passed a Windows 11
software-renderer smoke run on `avd`, using an isolated configuration and mocked
responses: long text and narrow layouts, same-folder `/new`, rename, archive,
folderless creation, settings and history search with Luna fallback.

The 140,199,373-byte ZIP has SHA-256
`ba050e173f233b6f3a881143dd21b22a611ba73237fcec74d7e920a41faeb9d0`.
GitHub's published digest matched before cleanup. Removed 20.5 GiB of Cargo
outputs, the verified local copies of both published Windows packages and the
temporary V8 download. Removed the Windows smoke-test binaries and ZIP as well;
validation logs and screenshots remain. No macOS release build was made.

## 15. Conversation scrolling, Windows redraw and link interactions

**Testing restriction:** Never launch or test this project's GUI on Sean's Mac,
including GUI test fixtures, unless the user explicitly says “test the GUI on
this Mac”. This persists across sessions. Use Windows hosts for GUI testing;
Mac source checks and Windows cross-builds remain permitted. See `AGENTS.md`.

Upward wheel gestures detach tail-following before virtual row measurements
change the transcript height. Small idle scrolls and scrollbar gestures detach.
Returning to the bottom or choosing “Jump to latest”
resumes tail-following. The pane still has no horizontal scrolling.

Windows redraws invalidate the complete frame, so retained presentation buffers
cannot leave stale or black regions after remote-display exposure. Focus,
restore, resize and scale changes request a frame. This uses existing redraw
events and adds no idle polling. Automatic renderer selection uses the software
renderer in Windows remote desktop sessions; explicit renderer overrides remain
available. The regression fixture simulates lost retained-buffer pixels; it
does not reproduce a particular Azure Virtual Desktop driver failure.

Web and file links share right-click “Copy link” and “Open in browser” actions;
file links also offer “Open file”. Copying a file destination resolves relative
paths against the conversation folder and preserves the line number. Browser
actions convert paths to encoded `file:` URLs. Rich paragraphs, wrapped links,
table cells and patch-file titles share the interaction. After a stationary
hover, a plain-text tooltip shows the destination; pointer movement, clicks,
scrolling or leaving the link dismiss it. Glyph hit-testing respects clipping.

Slint's shaped-glyph hit-test and renderer invalidation APIs are isolated in
`codex-rs/gui/src/window_runtime.rs`. The direct `i-slint-core` dependency must
stay aligned with Slint 1.18.1 when upgrading. Scoped recurrence invariants and
regressions are recorded in `codex-rs/gui/SPEC.md`.
The pinned core source under `third_party/slint` has a one-line correction to
accumulate hit-test results across every wrapped line. Remove the override when
an upstream Slint release supplies the correction.

## 16. Windows 0.1.2 release

Published `codex-gui-v0.1.2` from `e11ae5133722b031ebf4612b4c7557a04bb04652`
on October 6, 2026. Development verification completed with 620 GUI tests and
GUI clippy with warnings denied before the single Windows x64 release build.
The four packaged executables have verified x64 PE headers and static CRT;
the main executable uses the Windows GUI subsystem.

The finished package passed Windows 11 software-renderer checks on `avd` with
isolated configuration and mocked responses. A 10-pixel upward idle scroll
detached following and persisted through row measurements. Minimize/restore
produced a complete frame. Web/file context menus exposed the expected actions,
copied exact destinations, and opened a file in its own tab. Both destination
tooltips appeared after stationary hover and dismissed on one-pixel movement.
These Windows checks do not directly reproduce the original Azure Virtual
Desktop driver failure; the lost-buffer regression covers the recovery path.

The 140,276,460-byte ZIP has SHA-256
`2cf0e06e9256641de076ed430e1625792fbe6cc8e02cb3ed1c03bb99b6ad710d`.
The transferred Windows package and published GitHub asset matched this digest.
Only after publication verification, removed 17.0 GiB of Cargo outputs, the
local published ZIP, temporary V8 files, and Windows smoke binaries and ZIP.
Validation logs and screenshots remain. No macOS release build was made.
The Mac GUI testing prohibition is recorded in `AGENTS.md` and remains in force.
