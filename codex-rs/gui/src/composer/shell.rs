//! `!command`: runs a shell command in the thread's folder through
//! `thread/shellCommand`, like the TUI. The server runs it outside the
//! sandbox with the user's permissions and streams its output into the
//! thread as a command item, so the first command of a session asks for
//! confirmation.

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ThreadShellCommandParams;
use codex_app_server_protocol::ThreadShellCommandResponse;

use crate::app::AppController;
use crate::app::DialogRequest;
use crate::app::ThreadPhase;
use crate::backend::BackendError;
use crate::transcript::NoticeKind;

/// What a `!` message asks for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ShellInput {
    /// Run this command.
    Command(String),
    /// Only `!` was typed.
    Empty,
}

/// Parses a submitted message as a shell command: `!cmd`. Leading
/// whitespace is ignored; the command keeps its own spacing and newlines.
pub(crate) fn parse_shell_command(text: &str) -> Option<ShellInput> {
    let command = text.trim_start().strip_prefix('!')?.trim();
    if command.is_empty() {
        Some(ShellInput::Empty)
    } else {
        Some(ShellInput::Command(command.to_string()))
    }
}

/// Body of the one-time confirmation.
fn confirmation_message(command: &str, folder: &str) -> String {
    format!(
        "“{command}” runs in {folder} with your full user permissions, outside the sandbox and without approvals. Its output is added to this thread.\n\nCommands you start with ! run this way until you quit Codex."
    )
}

impl AppController {
    /// Runs `!command` for tab `index`; `typed` is restored into the input
    /// when the user cancels.
    pub(super) fn composer_run_shell(&mut self, index: usize, input: ShellInput, typed: String) {
        let ShellInput::Command(command) = input else {
            self.toast("Type a command after ! to run it in this thread's folder");
            return;
        };
        let Some(thread) = self.thread_tab(index) else {
            return;
        };
        if matches!(thread.phase, ThreadPhase::Closed | ThreadPhase::Error) {
            return;
        }
        if thread.thread_id.is_none() || thread.phase == ThreadPhase::Starting {
            self.toast("Wait for the thread to start before running a command");
            return;
        }
        let folder = crate::newtab::display_path(&thread.cwd);
        let tab_id = self.tabs[index].id;
        self.composer_set_text(String::new(), 0);
        if self.composer_shared.shell_confirmed {
            self.composer_send_shell(index, command);
            return;
        }
        self.show_dialog(
            DialogRequest::confirm(
                "Run a command outside the sandbox?",
                confirmation_message(&command, &folder),
            )
            .accept_label("Run")
            .destructive(),
            Box::new(move |app, accepted| {
                let Some(index) = app.tab_index_by_id(tab_id) else {
                    return;
                };
                if accepted.is_none() {
                    if app.active == Some(index) {
                        let cursor = typed.len();
                        app.composer_set_text(typed, cursor);
                    }
                    return;
                }
                app.composer_shared.shell_confirmed = true;
                app.composer_send_shell(index, command);
            }),
        );
    }

    fn composer_send_shell(&mut self, index: usize, command: String) {
        let Some(thread_id) = self
            .thread_tab(index)
            .and_then(|thread| thread.thread_id.clone())
        else {
            return;
        };
        let tab_id = self.tabs[index].id;
        self.backend.call(
            |request_id| ClientRequest::ThreadShellCommand {
                request_id,
                params: ThreadShellCommandParams {
                    thread_id,
                    command,
                    timeout_ms: None,
                },
            },
            move |app, result: Result<ThreadShellCommandResponse, BackendError>| {
                if let Err(err) = result
                    && let Some(index) = app.tab_index_by_id(tab_id)
                {
                    app.transcript_push_notice(
                        index,
                        NoticeKind::Error,
                        format!("Could not run the command: {}", err.user_message()),
                    );
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_bang_commands() {
        assert_eq!(
            parse_shell_command("!ls -la"),
            Some(ShellInput::Command("ls -la".to_string()))
        );
        assert_eq!(
            parse_shell_command("  ! git status \n"),
            Some(ShellInput::Command("git status".to_string()))
        );
        assert_eq!(
            parse_shell_command("!for f in *; do\n  echo $f\ndone"),
            Some(ShellInput::Command(
                "for f in *; do\n  echo $f\ndone".to_string()
            ))
        );
        assert_eq!(parse_shell_command("!"), Some(ShellInput::Empty));
        assert_eq!(parse_shell_command("!   "), Some(ShellInput::Empty));
    }

    #[test]
    fn other_messages_are_not_commands() {
        assert_eq!(parse_shell_command("ls"), None);
        assert_eq!(parse_shell_command("why does `!x` fail?"), None);
        assert_eq!(parse_shell_command(""), None);
        assert_eq!(parse_shell_command("/model"), None);
    }

    #[test]
    fn confirmation_names_command_and_folder() {
        let message = confirmation_message("rm -rf target", "/work/app");
        assert!(message.starts_with("“rm -rf target” runs in /work/app"));
        assert!(message.contains("outside the sandbox"));
    }
}
