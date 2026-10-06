//! Picker overlay (`PickerOverlay` in ui/newtab.slint): a modal that asks the
//! user to choose before an action runs. It hosts the folder-trust prompt and
//! the server folder browser (`crate::newtab`) and the review target picker
//! ([`super::review`]).
//!
//! One flow is on screen at a time. Flows opened meanwhile wait in a queue
//! instead of replacing it, so a choice the user has not made is never made
//! for them, and cancelling (button, Escape) only closes the flow. Each flow
//! renders its current step as a [`PickerView`] and handles the overlay's
//! events. Background results (directory listings, Git queries) find their
//! flow by id through [`AppController::picker_update`] and are dropped once
//! it closed.

use std::collections::VecDeque;
use std::rc::Rc;

use slint::ComponentHandle;
use slint::Model;
use slint::ModelRc;
use slint::VecModel;

use crate::app::AppController;
use crate::newtab::FolderBrowser;
use crate::newtab::TrustPrompt;
use crate::ui::PickerItem;
use crate::ui::PickerState;

use super::review::ReviewPicker;

/// A flow shown in the picker overlay.
pub(crate) enum PickerFlow {
    Trust(TrustPrompt),
    Browse(FolderBrowser),
    Review(ReviewPicker),
}

/// Buttons of the overlay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PickerButton {
    Accept,
    Secondary,
    Back,
    Cancel,
}

impl PickerButton {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "accept" => Some(Self::Accept),
            "secondary" => Some(Self::Secondary),
            "back" => Some(Self::Back),
            "cancel" => Some(Self::Cancel),
            _ => None,
        }
    }
}

/// Something the user did in the overlay.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PickerEvent {
    /// The text field changed.
    InputEdited(String),
    /// Enter in the text field, with the selected row at that moment.
    InputAccepted {
        text: String,
        selected: Option<usize>,
    },
    /// A row was run: a click on a menu row, a double click, or Enter.
    Activated(usize),
    /// A button, with the selected row and the text field at that moment.
    Button {
        button: PickerButton,
        selected: Option<usize>,
        input: String,
    },
}

/// What happens to a flow after it handled an event or a background result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PickerOutcome {
    /// Nothing visible changed.
    Unchanged,
    /// Re-render the step, keeping the text field and keyboard focus.
    Refresh,
    /// A new step: reset the text field, the selection and the focus.
    NewStep,
    /// Close the flow and show the next queued one.
    Close,
}

/// One step of a flow as the overlay shows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PickerView {
    pub(crate) title: String,
    pub(crate) message: String,
    pub(crate) input: Option<PickerInputView>,
    pub(crate) list: Option<PickerListView>,
    pub(crate) note: String,
    pub(crate) note_error: bool,
    /// Empty labels hide their buttons.
    pub(crate) back_label: String,
    pub(crate) cancel_label: String,
    pub(crate) secondary_label: String,
    pub(crate) accept_label: String,
    pub(crate) accept_enabled: bool,
}

/// The text field of a step.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PickerInputView {
    pub(crate) label: String,
    pub(crate) placeholder: String,
    /// Put into the field when the step starts.
    pub(crate) text: String,
}

/// The list of a step.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PickerListView {
    pub(crate) rows: Vec<PickerRowView>,
    /// Shown while `rows` is empty (loading, errors, no matches).
    pub(crate) placeholder: String,
    /// A click runs the row instead of selecting it.
    pub(crate) click_activates: bool,
    /// The accept button needs a selected row.
    pub(crate) accept_needs_selection: bool,
    /// Preselect the first row (filtered lists, so Enter picks it).
    pub(crate) select_first: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PickerRowView {
    pub(crate) title: String,
    pub(crate) detail: String,
    pub(crate) trailing: String,
}

impl PickerRowView {
    pub(crate) fn new(title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            detail: detail.into(),
            trailing: String::new(),
        }
    }

    pub(crate) fn trailing(mut self, trailing: impl Into<String>) -> Self {
        self.trailing = trailing.into();
        self
    }
}

type PickerUpdate = Box<dyn FnOnce(&mut AppController, &mut PickerFlow) -> PickerOutcome>;

/// The overlay's state: the flow on screen and the ones waiting.
#[derive(Default)]
pub(crate) struct PickerController {
    current: Option<(u64, PickerFlow)>,
    queue: VecDeque<(u64, PickerFlow)>,
    next_id: u64,
    /// The current flow is out of `current` while its handler runs.
    dispatching: bool,
    serial: i32,
    rows: Rc<VecModel<PickerItem>>,
}

/// Selected row to show after an update: `previous` stays selected unless
/// the rows changed, in which case the step's initial selection applies.
fn selection_after_update(
    previous: i32,
    rows_changed: bool,
    new_step: bool,
    row_count: usize,
    select_first: bool,
) -> i32 {
    let initial = if select_first && row_count > 0 { 0 } else { -1 };
    if new_step || rows_changed {
        return initial;
    }
    let in_range = usize::try_from(previous).is_ok_and(|previous| previous < row_count);
    if in_range { previous } else { initial }
}

impl AppController {
    pub(crate) fn picker_bind(&mut self) {
        let state = self.window.global::<PickerState>();
        state.set_items(ModelRc::from(self.newtab.picker.rows.clone()));
        state.on_input_edited(|text| {
            let text = text.to_string();
            crate::ui_thread::with_app(move |app| {
                app.picker_event(PickerEvent::InputEdited(text));
            });
        });
        state.on_input_accepted(|| {
            crate::ui_thread::with_app(|app| {
                let (text, selected) = app.picker_field_and_selection();
                app.picker_event(PickerEvent::InputAccepted { text, selected });
            });
        });
        state.on_item_activated(|index| {
            crate::ui_thread::with_app(move |app| {
                if let Ok(index) = usize::try_from(index) {
                    app.picker_event(PickerEvent::Activated(index));
                }
            });
        });
        state.on_button(|name| {
            let Some(button) = PickerButton::parse(name.as_str()) else {
                return;
            };
            crate::ui_thread::with_app(move |app| app.picker_press(button));
        });
    }

    /// Shows `flow`, or queues it behind the flow on screen. Returns the id
    /// that background results use to reach it.
    pub(crate) fn picker_open(&mut self, flow: PickerFlow) -> u64 {
        let picker = &mut self.newtab.picker;
        picker.next_id += 1;
        let id = picker.next_id;
        if picker.current.is_some() || picker.dispatching {
            picker.queue.push_back((id, flow));
        } else {
            picker.current = Some((id, flow));
            self.picker_render(/*new_step*/ true);
        }
        id
    }

    /// Applies a background result (or any change) to flow `id` while it is
    /// on screen or queued; a closed flow ignores it.
    pub(crate) fn picker_update(
        &mut self,
        id: u64,
        update: impl FnOnce(&mut AppController, &mut PickerFlow) -> PickerOutcome + 'static,
    ) {
        self.picker_update_boxed(id, Box::new(update));
    }

    fn picker_update_boxed(&mut self, id: u64, update: PickerUpdate) {
        let picker = &mut self.newtab.picker;
        if picker.dispatching {
            // A handler is running and re-renders when it returns; apply
            // this update right after it.
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                crate::ui_thread::with_app(move |app| app.picker_update_boxed(id, update));
            });
            return;
        }
        if picker
            .current
            .as_ref()
            .is_none_or(|(current, _)| *current != id)
        {
            // A queued flow gets its data before it is shown.
            if let Some(position) = picker.queue.iter().position(|(queued, _)| *queued == id)
                && let Some((_, mut flow)) = picker.queue.remove(position)
            {
                picker.dispatching = true;
                let outcome = update(self, &mut flow);
                self.newtab.picker.dispatching = false;
                if outcome != PickerOutcome::Close {
                    let queue = &mut self.newtab.picker.queue;
                    queue.insert(position.min(queue.len()), (id, flow));
                }
            }
            return;
        }
        let Some((_, mut flow)) = picker.current.take() else {
            return;
        };
        picker.dispatching = true;
        let outcome = update(self, &mut flow);
        self.newtab.picker.dispatching = false;
        match outcome {
            PickerOutcome::Close => self.picker_show_next(),
            PickerOutcome::Unchanged => self.newtab.picker.current = Some((id, flow)),
            PickerOutcome::Refresh | PickerOutcome::NewStep => {
                self.newtab.picker.current = Some((id, flow));
                self.picker_render(outcome == PickerOutcome::NewStep);
            }
        }
    }

    /// The text field and the selected row (when it is in range).
    fn picker_field_and_selection(&self) -> (String, Option<usize>) {
        let state = self.window.global::<PickerState>();
        let selected = usize::try_from(state.get_selected())
            .ok()
            .filter(|&index| index < self.newtab.picker.rows.row_count());
        (state.get_input_text().to_string(), selected)
    }

    fn picker_press(&mut self, button: PickerButton) {
        let (input, selected) = self.picker_field_and_selection();
        self.picker_event(PickerEvent::Button {
            button,
            selected,
            input,
        });
    }

    fn picker_event(&mut self, event: PickerEvent) {
        let Some(id) = self.newtab.picker.current.as_ref().map(|(id, _)| *id) else {
            return;
        };
        self.picker_update(id, move |app, flow| match flow {
            PickerFlow::Trust(prompt) => app.newtab_trust_event(prompt, &event),
            PickerFlow::Browse(browser) => app.newtab_browse_event(browser, id, event),
            PickerFlow::Review(review) => app.review_event(review, id, event),
        });
    }

    fn picker_show_next(&mut self) {
        let next = self.newtab.picker.queue.pop_front();
        let new_flow = next.is_some();
        self.newtab.picker.current = next;
        if new_flow {
            self.picker_render(/*new_step*/ true);
        } else {
            self.window.global::<PickerState>().set_open(false);
        }
    }

    fn picker_render(&mut self, new_step: bool) {
        let state = self.window.global::<PickerState>();
        let Some((_, flow)) = self.newtab.picker.current.as_ref() else {
            state.set_open(false);
            return;
        };
        let view = match flow {
            PickerFlow::Trust(prompt) => prompt.view(),
            PickerFlow::Browse(browser) => browser.view(&self.connection_label),
            PickerFlow::Review(review) => review.view(),
        };
        state.set_title(view.title.into());
        state.set_message(view.message.into());
        match view.input {
            Some(input) => {
                state.set_input_visible(true);
                state.set_input_label(input.label.into());
                state.set_input_placeholder(input.placeholder.into());
                if new_step {
                    state.set_input_text(input.text.into());
                }
            }
            None => {
                state.set_input_visible(false);
                if new_step {
                    state.set_input_text(Default::default());
                }
            }
        }
        let list = view.list.unwrap_or_default();
        state.set_list_visible(!list.rows.is_empty() || !list.placeholder.is_empty());
        state.set_list_placeholder(list.placeholder.into());
        state.set_click_activates(list.click_activates);
        state.set_accept_needs_selection(list.accept_needs_selection);
        let rows: Vec<PickerItem> = list
            .rows
            .into_iter()
            .map(|row| PickerItem {
                title: row.title.into(),
                detail: row.detail.into(),
                trailing: row.trailing.into(),
            })
            .collect();
        let model = &self.newtab.picker.rows;
        let rows_changed = model.row_count() != rows.len()
            || rows
                .iter()
                .enumerate()
                .any(|(index, row)| model.row_data(index).as_ref() != Some(row));
        let row_count = rows.len();
        crate::sidebar::sync_model(model, rows);
        state.set_selected(selection_after_update(
            state.get_selected(),
            rows_changed,
            new_step,
            row_count,
            list.select_first,
        ));
        state.set_note(view.note.into());
        state.set_note_error(view.note_error);
        state.set_back_label(view.back_label.into());
        state.set_cancel_label(view.cancel_label.into());
        state.set_secondary_label(view.secondary_label.into());
        state.set_accept_label(view.accept_label.into());
        state.set_accept_enabled(view.accept_enabled);
        if new_step {
            self.newtab.picker.serial = self.newtab.picker.serial.wrapping_add(1);
            state.set_serial(self.newtab.picker.serial);
        }
        state.set_open(true);
    }

    /// Drives the overlay from automation scripts: `["browse", path]` opens
    /// the server folder browser, `["input", text]`, `["enter"]`,
    /// `["activate", row]`, `["select", row]`, `["button", name]`.
    pub(crate) fn picker_automation(&mut self, args: &[String]) {
        let arg = |index: usize| args.get(index).map(String::as_str).unwrap_or_default();
        match arg(0) {
            "browse" => self.newtab_browse_server_folders(Some(arg(1).to_string())),
            "input" => {
                let text = arg(1).to_string();
                self.window
                    .global::<PickerState>()
                    .set_input_text(text.as_str().into());
                self.picker_event(PickerEvent::InputEdited(text));
            }
            "enter" => {
                let (text, selected) = self.picker_field_and_selection();
                self.picker_event(PickerEvent::InputAccepted { text, selected });
            }
            "activate" => {
                if let Ok(row) = arg(1).parse::<usize>() {
                    self.picker_event(PickerEvent::Activated(row));
                }
            }
            "select" => {
                if let Ok(row) = arg(1).parse::<i32>() {
                    self.window.global::<PickerState>().set_selected(row);
                }
            }
            "button" => {
                if let Some(button) = PickerButton::parse(arg(1)) {
                    self.picker_press(button);
                }
            }
            other => tracing::warn!(command = other, "unknown picker automation command"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn buttons_parse_by_name() {
        assert_eq!(PickerButton::parse("accept"), Some(PickerButton::Accept));
        assert_eq!(
            PickerButton::parse("secondary"),
            Some(PickerButton::Secondary)
        );
        assert_eq!(PickerButton::parse("back"), Some(PickerButton::Back));
        assert_eq!(PickerButton::parse("cancel"), Some(PickerButton::Cancel));
        assert_eq!(PickerButton::parse("ok"), None);
    }

    #[test]
    fn selection_resets_on_new_steps_and_changed_rows() {
        // A new step starts from the step's initial selection.
        assert_eq!(selection_after_update(3, false, true, 5, false), -1);
        assert_eq!(selection_after_update(3, false, true, 5, true), 0);
        // Changed rows (a new filter) select the first match again.
        assert_eq!(selection_after_update(3, true, false, 5, true), 0);
        assert_eq!(selection_after_update(3, true, false, 0, true), -1);
        // Unchanged rows keep the user's selection while it is in range.
        assert_eq!(selection_after_update(3, false, false, 5, true), 3);
        assert_eq!(selection_after_update(7, false, false, 5, false), -1);
        assert_eq!(selection_after_update(-1, false, false, 5, false), -1);
    }
}
