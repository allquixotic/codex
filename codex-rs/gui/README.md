# codex-gui

A native, tabbed desktop front end for Codex, built with [Slint](https://slint.dev).
It runs the Codex app-server **in-process** (the same embedded path the TUI uses),
so there is one process, one Tokio runtime, and no socket or open port by
default. See [`GUI.md`](../../GUI.md) for the design plan and
[`docs/gui.md`](../../docs/gui.md) for the user guide.

## Build and run

```bash
cd codex-rs
cargo run -p codex-gui                 # opens the New Tab page
cargo run -p codex-gui -- ~/src/proj   # starts a thread in a folder
cargo run -p codex-gui -- --resume <thread-id>
cargo run -p codex-gui -- --renderer software   # force the CPU renderer
cargo run -p codex-gui -- --remote ws://127.0.0.1:4500   # use an external app-server
```

`codex-gui` is also the `apply_patch` / sandbox helper executable for the
threads it runs (arg0 dispatch happens before any UI code), exactly like the
`codex` multitool.

## Architecture

```
main thread                         Tokio runtime (worker threads)
┌───────────────────────────┐       ┌──────────────────────────────────────┐
│ Slint event loop          │ batch │ backend pump: owns AppServerClient,   │
│ AppController (app.rs)    │◄──────│ drains events, coalesces deltas per   │
│  tabs, feature state,     │ post  │ 16 ms frame, one UI post per batch    │
│  Slint models/globals     │       │                                      │
│ Slint callbacks ──────────┼──────►│ request tasks: Backend::call/fire     │
│   ui_thread::with_app     │ spawn │ embedded app-server + codex-core      │
└───────────────────────────┘       └──────────────────────────────────────┘
```

| Module | Responsibility |
|---|---|
| `main.rs`, `lib.rs` | arg0 dispatch that keeps the main thread for the UI, CLI (`--help` and errors shown without a console on Windows), renderer selection, window-system event hooks, event loop, shutdown |
| `platform.rs` | OS integration Slint lacks: Cmd+Q / Dock › Quit / logout on macOS, the login-shell `PATH` for Finder launches, the app bundle id for notifications, the parent console on Windows, the OpenGL probe on Linux |
| `startup.rs` | embedded app-server startup (config, cloud bundle, environments, state DB, tracing) and restart after provider changes; file logging for startup failures and remote mode |
| `connection.rs` | opt-in daemon (`unix://`) or remote (`ws://`, `wss://`) app-server targets, resolved from `--remote` or Settings › Connection |
| `backend.rs` | `Backend` handle: typed requests, server-request answers, restart, reconnect, shutdown; the event pump and delta coalescing |
| `ui_thread.rs` | access to the UI-thread `AppController` from callbacks (`with_app`) and from Tokio (`post`) |
| `app.rs` | tabs, event routing to features, tab strip, drawers, dialogs, toasts, theme, window lifecycle, command-line startup (sign-in and trust checks) |
| `threads.rs`, `threads/` | thread start/resume/fork/close and the input path (start, steer, or queue a turn); `recap`, `side` chats, `worktree` |
| `session.rs` | request builders for common app-server RPCs |
| `transcript/` | block model, markdown rendering, streaming, paged history, copy/export |
| `composer/` | input, `@` mentions, `/` palette, attachments, file drops, model/effort/permission pickers |
| `approvals/` | approvals, user-input questions, MCP elicitations |
| `settings/` | settings tab: common, schema-driven "all settings", raw TOML, import, account, MCP, skills, plugins, hooks, features, appearance, keyboard, connection, Windows sandbox, diagnostics, feedback; `settings/bedrock` is the providers page (Amazon Bedrock, local models) |
| `files/` | file viewer and diff viewer tabs, find, open externally |
| `sidebar.rs`, `info.rs` + `info/`, `newtab.rs` | thread list, info pane (`info/terminals`: background terminals), new-tab page (folder trust check) |
| `xtab/` | cross-tab messaging tools, mailbox, wait-for-reply |
| `notify.rs` | desktop notifications while the window is in the background |
| `shortcuts.rs` | global keymap (user-overridable) |
| `prefs.rs` | GUI-only preferences in `$CODEX_HOME/gui.json` (invalid values are reported and replaced one by one) |
| `automation.rs` | scripted UI runs for tests (`CODEX_GUI_AUTOMATION`) |
| `ui/*.slint` | one view + one global per feature; `ui/app.slint` places them and re-exports every global |

Rules that keep the UI responsive:

- The UI thread never blocks on I/O. Slint callbacks call
  `ui_thread::with_app(|app| ...)`; work runs through `Backend::call`, whose
  completion closure runs back on the UI thread.
- Every server request (approval, question, dynamic tool call) is answered,
  even on error, so turns never hang.
- Streaming deltas are coalesced per frame in the backend, and the transcript
  only re-renders the unfinished trailing block.

## Testing

```bash
cargo test -p codex-gui
cargo clippy -p codex-gui --tests
```

End-to-end runs use a mock Responses API server and a scripted UI session:

```bash
SP=$(mktemp -d); mkdir -p $SP/home $SP/project
python3 gui/dev/mock_responses.py --port 18080 --write-config $SP/home &
cat > $SP/script.json <<EOF
[{"wait_ready": 60000}, {"new_thread": "$SP/project"}, {"wait_idle": 30000},
 {"send": "markdown please"}, {"wait": 300}, {"wait_idle": 30000},
 {"snapshot": "$SP/shot.png"}, {"quit": true}]
EOF
CODEX_HOME=$SP/home CODEX_GUI_AUTOMATION=$SP/script.json cargo run -p codex-gui
```

The mock's reply depends on the message prefix (`markdown`, `run <cmd>`,
`patch`, `plan`, `ask`, `tab`, `slow`); see the docstring in
`dev/mock_responses.py`. Automation steps are documented in
`src/automation.rs`.

## Packaging

- macOS: `packaging/macos/bundle-app.sh <binary> <out-dir> <version>
  [--bundle-id ID] [--helper PATH]...` builds an unsigned `Codex.app`;
  helpers are copied next to `codex-gui` in `Contents/MacOS`.
- `.github/workflows/rust-release-gui.yml` builds macOS, Windows, and Linux
  artifacts on release tags. Each holds `codex-gui` plus the helper
  executables the embedded runtime finds next to it: `codex-code-mode-host`,
  and on Windows `codex-windows-sandbox-setup` and `codex-command-runner`.
  Linux sandboxing uses the system `bwrap` (bubblewrap).
- The window icon is `ui/assets/icon.png`, embedded with `include_bytes!`
  from `src/app.rs` (not a Slint `@image-url`, which would embed the build
  script's absolute path and break Bazel's sandboxed compile).

## License note

Slint is used under the Slint Royalty-free Desktop License 2.0, which requires
the "Made with Slint" attribution: **Help › About Codex** shows the
`AboutSlint` widget. Codex sources remain Apache-2.0.
