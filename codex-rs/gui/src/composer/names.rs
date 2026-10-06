//! Names given with `/new <name>` and `/fork <name>` (as in the TUI, the
//! argument names the new thread).
//!
//! The thread does not exist yet when the command runs: `/new` starts in
//! the current folder immediately, and
//! `/fork` waits for `thread/fork`. The name is kept per tab and applied with
//! `thread/name/set` once the tab's thread is attached.

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ThreadSetNameParams;
use codex_app_server_protocol::ThreadSetNameResponse;

use crate::app::AppController;
use crate::app::TabId;
use crate::app::TabKind;
use crate::app::ThreadPhase;
use crate::backend::BackendError;

/// A name waiting for its tab's thread.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingName {
    pub(crate) name: String,
    /// The tab became the new thread (rather than, say, the "New tab" page
    /// opening an existing thread from the sidebar).
    pub(crate) claimed: bool,
}

/// What a pending name's tab looks like now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NamedTab {
    Gone,
    NewTabPage,
    /// A thread tab; `attached` once its thread id is known and it is no
    /// longer starting.
    Thread {
        has_thread_id: bool,
        attached: bool,
    },
    Other,
}

/// What to do with a pending name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NameStep {
    /// Still waiting (the "New tab" page or a starting thread).
    Wait,
    /// The tab is the new thread: show the name, keep waiting.
    Claim,
    /// The thread is attached: send `thread/name/set`.
    Apply,
    /// The tab is gone or holds something else.
    Drop,
}

pub(crate) fn name_step(tab: NamedTab, claimed: bool) -> NameStep {
    match tab {
        NamedTab::Gone | NamedTab::Other => NameStep::Drop,
        NamedTab::NewTabPage => NameStep::Wait,
        NamedTab::Thread { attached: true, .. } if claimed => NameStep::Apply,
        // A thread that already had an id when it replaced the page is an
        // existing thread (opened from the sidebar), not the new one.
        NamedTab::Thread {
            has_thread_id: true,
            ..
        } if !claimed => NameStep::Drop,
        NamedTab::Thread { .. } if claimed => NameStep::Wait,
        NamedTab::Thread { .. } => NameStep::Claim,
    }
}

impl AppController {
    /// Remembers `name` for the thread that tab `tab_id` will hold.
    /// `claimed` when the tab already is that thread (a fork).
    pub(super) fn composer_name_pending_thread(
        &mut self,
        tab_id: TabId,
        name: String,
        claimed: bool,
    ) {
        self.composer_shared
            .pending_names
            .insert(tab_id, PendingName { name, claimed });
        self.composer_apply_pending_names();
    }

    /// Advances every pending name (see [`name_step`]).
    pub(super) fn composer_apply_pending_names(&mut self) {
        if self.composer_shared.pending_names.is_empty() {
            return;
        }
        let pending: Vec<(TabId, PendingName)> = self
            .composer_shared
            .pending_names
            .iter()
            .map(|(tab_id, pending)| (*tab_id, pending.clone()))
            .collect();
        let mut renamed = false;
        for (tab_id, pending) in pending {
            let index = self.tab_index_by_id(tab_id);
            let tab = match index.and_then(|index| self.tabs.get(index)) {
                None => NamedTab::Gone,
                Some(tab) => match &tab.kind {
                    TabKind::NewTab => NamedTab::NewTabPage,
                    TabKind::Thread(thread) => NamedTab::Thread {
                        has_thread_id: thread.thread_id.is_some(),
                        attached: thread.thread_id.is_some()
                            && thread.phase != ThreadPhase::Starting,
                    },
                    _ => NamedTab::Other,
                },
            };
            match name_step(tab, pending.claimed) {
                NameStep::Wait => {}
                NameStep::Drop => {
                    self.composer_shared.pending_names.remove(&tab_id);
                }
                NameStep::Claim => {
                    if let Some(entry) = self.composer_shared.pending_names.get_mut(&tab_id) {
                        entry.claimed = true;
                    }
                    if let Some(thread) = index.and_then(|index| self.thread_tab_mut(index)) {
                        thread.name = Some(pending.name);
                        renamed = true;
                    }
                }
                NameStep::Apply => {
                    self.composer_shared.pending_names.remove(&tab_id);
                    let Some(thread) = index.and_then(|index| self.thread_tab_mut(index)) else {
                        continue;
                    };
                    thread.name = Some(pending.name.clone());
                    renamed = true;
                    let Some(thread_id) = thread.thread_id.clone() else {
                        continue;
                    };
                    let name = pending.name;
                    self.backend.call(
                        |request_id| ClientRequest::ThreadSetName {
                            request_id,
                            params: ThreadSetNameParams { thread_id, name },
                        },
                        |app, result: Result<ThreadSetNameResponse, BackendError>| {
                            if let Err(err) = result {
                                app.toast(format!(
                                    "Could not name the thread: {}",
                                    err.user_message()
                                ));
                            }
                        },
                    );
                }
            }
        }
        if renamed {
            self.refresh_tabs();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const STARTING: NamedTab = NamedTab::Thread {
        has_thread_id: false,
        attached: false,
    };
    const ATTACHED: NamedTab = NamedTab::Thread {
        has_thread_id: true,
        attached: true,
    };

    #[test]
    fn new_thread_name_waits_for_the_folder_then_the_thread() {
        // `/new <name>`: the "New tab" page waits for a folder...
        assert_eq!(
            name_step(NamedTab::NewTabPage, /*claimed*/ false),
            NameStep::Wait
        );
        // ...the starting thread takes the name...
        assert_eq!(name_step(STARTING, /*claimed*/ false), NameStep::Claim);
        assert_eq!(name_step(STARTING, /*claimed*/ true), NameStep::Wait);
        // ...and it is sent once the thread is attached.
        assert_eq!(name_step(ATTACHED, /*claimed*/ true), NameStep::Apply);
    }

    #[test]
    fn existing_threads_and_closed_tabs_drop_the_name() {
        // The page opened a saved thread instead of starting a new one.
        let resuming = NamedTab::Thread {
            has_thread_id: true,
            attached: false,
        };
        assert_eq!(name_step(resuming, /*claimed*/ false), NameStep::Drop);
        assert_eq!(name_step(ATTACHED, /*claimed*/ false), NameStep::Drop);
        assert_eq!(name_step(NamedTab::Gone, /*claimed*/ true), NameStep::Drop);
        assert_eq!(
            name_step(NamedTab::Other, /*claimed*/ false),
            NameStep::Drop
        );
        // A fork is claimed from the start and waits for `thread/fork`.
        assert_eq!(name_step(resuming, /*claimed*/ true), NameStep::Wait);
    }
}
