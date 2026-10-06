# Codex desktop app (`codex-gui`)

`codex-gui` is a native desktop front end for Codex with browser-style tabs.
Each tab is one agent thread bound to one folder; many agents can work at the
same time. It runs the Codex app-server inside its own process, so it needs no
browser engine, opens no network port by default, and works on machines
without a GPU (for example Azure Virtual Desktop) through software rendering.

## Running it

Build from source (see [install.md](install.md) for the toolchain):

```bash
cd codex-rs
cargo build --release -p codex-gui
./target/release/codex-gui                  # New Tab page
./target/release/codex-gui ~/src/my-repo    # start a thread in a folder
./target/release/codex-gui --resume <id>    # reopen a thread
```

A folder or thread given on the command line opens the same way as from the
New Tab page: if Codex needs you to sign in first, Settings › Account opens
and the folder or thread follows once you are signed in, and a project
folder you have not trusted yet asks **Trust this folder?** first.

Other options:

| Option | Meaning |
|---|---|
| `--renderer auto` | OpenGL renderer, falling back to the CPU renderer when OpenGL is unavailable (default; lowest CPU use) |
| `--renderer software` | Always render on the CPU (lowest memory use) |
| `--renderer gpu` | WGPU renderer (Metal, Direct3D 12, Vulkan), also allowing software adapters such as Windows WARP |
| `-c key=value` | Config overrides, same syntax as the CLI |
| `--remote ADDR` | Use another app-server: `embedded`, `unix://` (the local Codex daemon), `ws://host:port`, or `wss://host:port`. Overrides the connection saved under Settings › Connection |
| `--remote-auth-token-env VAR` | Environment variable holding the token for `--remote` (read only for `ws://` and `wss://`) |

The renderer can also be chosen under **Settings › Appearance**, and the
app-server connection under **Settings › Connection** (saved in `gui.json`,
used from the next start). If a saved or requested connection cannot be used
(for example its token variable is not set, which is common when the app is
started from Finder or the Start menu instead of a terminal), the window
still opens and shows the problem with **Retry** (which also picks up a
connection you changed in Settings), **Use the embedded server** (for this
session; the saved connection is kept), and **Open settings**.

On Windows, `--help`, `--version` and argument errors print to the console
the app was started from, or appear in a message box when there is none.

## The window

- **Tabs.** `Ctrl/⌘+T` opens a New Tab page where you pick a folder (recent
  folders are listed). Each thread tab shows a status dot: blue while working,
  amber when it needs your decision, red on errors, green for unread replies.
  Right-click a tab to rename, fork, compact, review, create `AGENTS.md`,
  export as Markdown, copy the thread id, or archive it. Middle-click closes.
  Drag a tab to move it. With many tabs, tabs get narrower and then the strip
  scrolls (mouse wheel or trackpad); the **⌄** button at its end lists every
  tab. "Close other tabs" asks once for all tabs that are still working.
- **Sidebar.** All your threads grouped by folder, with search and an archived
  view. Click to open; right-click for more actions. In a narrow window the
  sidebar and the info pane open over the content instead of beside it
  (`Esc` or a click outside closes them).
- **Transcript.** Agent replies render as formatted text with headings, lists,
  tables and code blocks (each with Copy). Commands, file edits, tool calls,
  plans and sub-agents appear as compact cards you can expand. Long threads
  load older history as you scroll up, so memory does not grow with length.
- **Composer.** `Enter` sends and `Shift+Enter` adds a line (configurable).
  Type `@` to mention a file, `/` for commands, `$` for skills. Paste or
  attach images. While the agent works, typing steers the running turn (or
  queues the message, see Settings › Appearance); `Esc` interrupts. The
  toolbar picks the model, reasoning effort, permissions, and Plan mode.
- **Approvals.** When the agent needs permission to run a command, edit files,
  or answer a question, a card appears above the composer. Keys: `Y` accept,
  `A` always for this session where offered, `Esc` decline and tell Codex what
  to do differently.
- **Info pane.** Model and permissions, context window usage, the turn's plan
  and file changes (with "Open diff"), sub-agents, queued messages, the
  thread goal, and cross-tab messages.
- **File and diff tabs.** Click a file path in a reply, or use
  File › Open File…, to view a file with selectable text, line numbers, find
  (`Ctrl/⌘+F`), and live reload. Diffs open in their own tab with per-file
  collapse and word-level highlights.

## Settings

Settings is a tab (`Ctrl/⌘+,`). Changes are written to your `config.toml`
through the app-server, keeping comments and formatting, and each value shows
where it comes from (your config, a project's `.codex/config.toml`, or a
managed policy that locks it).

| Page | What it covers |
|---|---|
| Common | model, reasoning effort, approvals, sandbox, web search, summaries |
| All settings | every config key, generated from the config schema, with search |
| config.toml | the raw file, validated before saving (works even if Codex cannot start) |
| Import | bring your setup over from other coding agents (Claude Code, Cursor) |
| Account | sign in with ChatGPT or an API key, sign out |
| Providers | Amazon Bedrock setup and local models (Ollama, LM Studio) |
| MCP servers, Skills, Plugins, Hooks, Features | enable, disable, add, trust |
| Appearance | theme, font size, renderer, Enter behavior, notifications |
| Keyboard | change or remove shortcuts |
| Connection | embedded app-server (default), the local daemon, or a remote app-server and its token variable |
| Windows sandbox | set up the Windows sandbox (Windows only) |
| Diagnostics | config layers, requirements, versions, log and Codex home folders |
| Send feedback | report a problem, optionally with logs |

## Amazon Bedrock

Settings › Providers sets Codex up to use Amazon Bedrock in a few clicks:

1. Choose the endpoint: **Mantle** (`amazon-bedrock`) or **Runtime**
   (`amazon-bedrock-runtime`).
2. Choose credentials: a detected AWS profile, environment credentials, a
   profile name, access keys, or a Bedrock API key.
3. Pick the region (GovCloud regions show a warning to review your
   organization's requirements).
4. **Validate**, then **Apply**. Codex restarts its embedded server on the new
   provider and lists the Bedrock models so you can pick one.

**Switch back to OpenAI** undoes the change. The same page lists models from a
running Ollama or LM Studio server.

## Agents talking to each other

When cross-tab messaging is on (Settings › Appearance, on by default, and per
tab in the info pane), agents in new tabs get three tools: list the open tabs,
send a message to another tab (optionally waiting for its reply), and read
their own mailbox. An agent's message is never delivered silently: the
receiving tab shows a card with the message and lets you **Deliver** it,
**Deliver and allow** messages from that tab for the rest of the session, or
**Decline** it (the sending agent is told you declined). Messages from a tab to
one that can do more (for example from a read-only tab to a Full access tab,
to a tab that never asks for approval, or to one that can write in another
folder) always ask, even if you allowed that pair. To keep agents from keeping
each other busy, an agent can send at most 5 messages per turn, a tab holds at
most 10 undelivered agent messages, and a chain of agent-to-agent messages
asks you again after 3 automatic hops. Threads you reopen from history start
with receiving turned off; turn it on in the info pane. Delivered messages go
through the receiving thread's queue, so a busy agent gets them when its
current turn ends. You can also forward any reply yourself with
**Send to tab…**. Messages are recorded in `$CODEX_HOME/gui/mailbox.jsonl`,
readable only by you.

## Keyboard

| Shortcut | Action |
|---|---|
| `Ctrl/⌘+T` | New tab |
| `Ctrl/⌘+W` | Close tab |
| `Ctrl+Tab`, `Ctrl+Shift+Tab` (`⌃Tab`, `⌃⇧Tab` on macOS, where `⌘Tab` switches apps) | Next / previous tab |
| `Ctrl/⌘+1…9` | Go to tab (9 = last) |
| `Ctrl/⌘+,` | Settings |
| `Ctrl/⌘+B` | Toggle sidebar |
| `Ctrl/⌘+Shift+I` | Toggle info pane |
| `Ctrl/⌘+O` | Open file |
| `Esc` | Close a drawer or dialog; otherwise interrupt the running turn |

In dialogs, `Enter` confirms and `Esc` cancels. Shortcuts can be changed
under Help › Keyboard Shortcuts (Settings › Keyboard).

Quitting (File › Quit, `⌘Q` or Dock › Quit on macOS, or closing the window)
asks first when threads are still working, then stops them and shuts the
app-server down cleanly. When you log out or shut down the Mac, Codex stops
running turns and quits without asking.

## Machines without a GPU

The default renderer falls back to the CPU renderer automatically when
OpenGL is unavailable: on Windows when there is no OpenGL driver, and on
Linux when the OpenGL libraries are missing or OpenGL fails as the window
opens (Codex then restarts itself with `--renderer software`). On Windows
VMs such as Azure Virtual Desktop you can also try `--renderer gpu`, which
lets the WGPU renderer run on the D3D12 WARP software adapter; compare it
with `--renderer software` and keep whichever feels smoother.

## Files and logs

- Preferences for the app itself: `$CODEX_HOME/gui.json`. A value Codex
  cannot read (for example a hand-edited typo) is reported in a banner and
  replaced by its default; the other values are kept. A file that is not
  valid JSON is copied to `gui.json.invalid-<time>` before defaults are used.
- Logs: `$CODEX_HOME/log/codex-gui.*.log` (rotated daily, 7 kept), including
  the reason when Codex cannot start; Help › Open Log Folder (or Settings ›
  Diagnostics) opens the folder in Finder, Explorer, or your file manager.
- Threads are the same as the CLI's: `codex resume` sees threads started in
  the app and the other way around.

## Notes

- The app embeds its own app-server. If you also run the TUI with its shared
  daemon, avoid opening the same thread in both at once (only one writer may
  own a thread); `--remote unix://` makes the app use the daemon instead.
- macOS: when Codex.app is started from Finder or the Dock, it takes `PATH`
  from your login shell, so MCP servers and hooks find tools such as `npx`,
  `uvx` or `docker`. Only login files are read (`~/.zprofile`, `~/.zshenv`,
  `~/.zlogin`, or `~/.bash_profile` / `~/.profile`), not `~/.zshrc`; put
  `PATH` changes there, or start Codex from a terminal.
- Desktop notifications (a reply finished, or a thread needs you) appear
  while the window is minimized or another app is in front. On macOS they
  come from Codex.app; on Windows they currently appear as coming from
  Windows PowerShell.
- Release downloads include the helper programs Codex runs next to
  `codex-gui` (`codex-code-mode-host`, and on Windows the sandbox helpers);
  keep them in the same folder. On Linux, sandboxing uses `bwrap`
  (install the `bubblewrap` package).
- Help › About shows the "Made with Slint" attribution required by Slint's
  royalty-free desktop license.
