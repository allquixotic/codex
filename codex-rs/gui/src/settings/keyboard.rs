//! Settings › Keyboard: the global shortcut keymap (`prefs.keymap`).
//!
//! Lists every action from [`crate::shortcuts::actions`] with its effective
//! binding, records new key combinations, reports conflicts (two actions
//! on the same keys; the first in display order wins, like
//! [`crate::shortcuts::Keymap::lookup`]), and resets or unbinds actions.
//! Overrides equal to the default are dropped so `gui.json` only keeps real
//! customizations.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::time::Duration;

use slint::ComponentHandle;
use slint::SharedString;
use slint::platform::Key;
use slint::platform::WindowEvent;

use super::RowList;
use crate::app::AppController;
use crate::app::DialogRequest;
use crate::shortcuts::KeyPress;
use crate::shortcuts::action_label;
use crate::shortcuts::actions;
use crate::shortcuts::binding_for_press;
use crate::shortcuts::canonical_binding;
use crate::shortcuts::default_binding;
use crate::shortcuts::display_binding;
use crate::ui::SettingsState;
use crate::ui::ShortcutRow;

/// Chords that text fields need for editing; binding them globally would
/// break copy, paste, select all, and undo everywhere.
const RESERVED_BINDINGS: &[&str] = &[
    "Mod+A",
    "Mod+C",
    "Mod+V",
    "Mod+X",
    "Mod+Z",
    "Mod+Y",
    "Mod+Shift+Z",
];

/// Banner tones understood by `Banner`.
const TONE_INFO: i32 = 0;
const TONE_SUCCESS: i32 = 1;
const TONE_WARNING: i32 = 2;

#[derive(Default)]
pub(crate) struct KeyboardState {
    rows: RowList<ShortcutData, ShortcutRow>,
    /// Action whose new binding is being recorded.
    recording: Option<String>,
    /// A recorded binding that another action already uses.
    pending: Option<PendingBinding>,
    notice: Option<(String, i32)>,
    revision: i32,
}

/// A recorded binding waiting for the user to resolve a conflict.
#[derive(Clone, Debug, PartialEq)]
struct PendingBinding {
    action: String,
    binding: String,
    /// Actions to unbind when the user reassigns the keys.
    conflicts: Vec<String>,
}

/// Plain mirror of the Slint `ShortcutRow`.
#[derive(Clone, Debug, Default, PartialEq)]
struct ShortcutData {
    action: String,
    label: String,
    binding: String,
    default_binding: String,
    custom: bool,
    unbound: bool,
    recording: bool,
    warning: String,
    conflict_action: String,
    conflict_label: String,
}

impl ShortcutData {
    fn to_slint(&self, revision: i32) -> ShortcutRow {
        ShortcutRow {
            action: self.action.as_str().into(),
            label: self.label.as_str().into(),
            binding: self.binding.as_str().into(),
            default_binding: self.default_binding.as_str().into(),
            custom: self.custom,
            unbound: self.unbound,
            recording: self.recording,
            warning: self.warning.as_str().into(),
            conflict_action: self.conflict_action.as_str().into(),
            conflict_label: self.conflict_label.as_str().into(),
            revision,
        }
    }
}

/// Binding in effect for one action.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Effective {
    pub(crate) action: &'static str,
    /// Canonical binding; `None` when unbound.
    pub(crate) binding: Option<String>,
    /// The user changed this action (including unbinding it).
    pub(crate) custom: bool,
    /// An override in `gui.json` that does not parse; the default applies.
    pub(crate) invalid: Option<String>,
}

/// Effective bindings for every action, in display order. Mirrors
/// [`crate::shortcuts::Keymap::new`]: empty overrides unbind, invalid ones
/// fall back to the default.
pub(crate) fn effective_bindings(overrides: &BTreeMap<String, String>) -> Vec<Effective> {
    actions()
        .map(|action| {
            let default =
                default_binding(action).and_then(|binding| canonical_binding(binding).ok());
            match overrides.get(action) {
                None => Effective {
                    action,
                    binding: default,
                    custom: false,
                    invalid: None,
                },
                Some(binding) if binding.trim().is_empty() => Effective {
                    action,
                    binding: None,
                    custom: true,
                    invalid: None,
                },
                Some(binding) => match canonical_binding(binding) {
                    Ok(canonical) => Effective {
                        action,
                        custom: Some(&canonical) != default.as_ref(),
                        binding: Some(canonical),
                        invalid: None,
                    },
                    Err(err) => Effective {
                        action,
                        binding: default,
                        custom: false,
                        invalid: Some(format!("\"{binding}\" in gui.json: {err}")),
                    },
                },
            }
        })
        .collect()
}

/// Other actions whose effective binding is `binding`, in display order.
pub(crate) fn conflicting_actions(
    effective: &[Effective],
    action: &str,
    binding: &str,
) -> Vec<&'static str> {
    effective
        .iter()
        .filter(|entry| entry.action != action && entry.binding.as_deref() == Some(binding))
        .map(|entry| entry.action)
        .collect()
}

/// Sets `action` to `binding` (`None` unbinds), dropping the override when
/// it matches the default.
pub(crate) fn set_binding(
    overrides: &mut BTreeMap<String, String>,
    action: &str,
    binding: Option<&str>,
) {
    let default = default_binding(action).and_then(|binding| canonical_binding(binding).ok());
    match binding {
        Some(binding) if default.as_deref() == Some(binding) => {
            overrides.remove(action);
        }
        Some(binding) => {
            overrides.insert(action.to_string(), binding.to_string());
        }
        None => {
            overrides.insert(action.to_string(), String::new());
        }
    }
}

/// What a key press means while recording.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Recorded {
    /// A modifier on its own; keep waiting for the rest of the chord.
    Waiting,
    /// Esc without modifiers ends recording without changes.
    Cancel,
    /// The keys cannot be used; the message explains why.
    Rejected(String),
    /// Canonical binding to assign.
    Binding(String),
}

/// Interprets a key press captured by the Keyboard page.
pub(crate) fn record_press(press: &KeyPress) -> Recorded {
    let modifiers = press.primary || press.secondary || press.alt;
    if press.text == "\u{1b}" && !modifiers && !press.shift {
        return Recorded::Cancel;
    }
    if is_modifier_key(&press.text) {
        return Recorded::Waiting;
    }
    let Some(binding) = binding_for_press(press) else {
        return Recorded::Rejected("That key cannot be used in a shortcut.".to_string());
    };
    let key = binding
        .rsplit_once('+')
        .map_or(binding.as_str(), |(_, key)| key);
    let function_key = key.len() >= 2 && key.starts_with('F') && key[1..].parse::<u8>().is_ok();
    if !modifiers && !function_key {
        let add = if cfg!(target_os = "macos") {
            "Add ⌘, ⌃, or ⌥"
        } else {
            "Add Ctrl or Alt"
        };
        return Recorded::Rejected(format!(
            "{add}: {} on its own would get in the way of typing.",
            display_binding(&binding)
        ));
    }
    if RESERVED_BINDINGS.contains(&binding.as_str()) {
        return Recorded::Rejected(format!(
            "{} is used for editing text (copy, paste, select all, undo) and cannot be assigned.",
            display_binding(&binding)
        ));
    }
    Recorded::Binding(binding)
}

/// Slint's codes for Shift, Control, Alt, AltGr, Caps Lock, and Meta
/// (left and right).
fn is_modifier_key(text: &str) -> bool {
    let mut chars = text.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some('\u{10}'..='\u{18}'), None)
    )
}

/// Builds the page rows from the effective keymap.
fn shortcut_rows(
    overrides: &BTreeMap<String, String>,
    recording: Option<&str>,
) -> Vec<ShortcutData> {
    let effective = effective_bindings(overrides);
    effective
        .iter()
        .map(|entry| {
            let default = default_binding(entry.action).unwrap_or_default();
            let mut row = ShortcutData {
                action: entry.action.to_string(),
                label: action_label(entry.action).to_string(),
                binding: display_binding(entry.binding.as_deref().unwrap_or_default()),
                default_binding: display_binding(default),
                custom: entry.custom || entry.invalid.is_some(),
                unbound: entry.binding.is_none(),
                recording: recording == Some(entry.action),
                ..ShortcutData::default()
            };
            if let Some(invalid) = &entry.invalid {
                row.warning = format!("Ignoring {invalid}. The default applies.");
            } else if let Some(binding) = entry.binding.as_deref() {
                let others = conflicting_actions(&effective, entry.action, binding);
                if let Some(first) = others.first() {
                    let names: Vec<String> = others
                        .iter()
                        .map(|other| format!("“{}”", action_label(other)))
                        .collect();
                    // `Keymap::lookup` takes the first match in display order.
                    let winner = effective
                        .iter()
                        .find(|candidate| candidate.binding.as_deref() == Some(binding))
                        .map(|candidate| candidate.action);
                    row.warning = format!(
                        "{} is also assigned to {}.",
                        display_binding(binding),
                        names.join(", ")
                    );
                    if winner != Some(entry.action) {
                        row.warning
                            .push_str(" This action does not run until the conflict is resolved.");
                    }
                    row.conflict_action = (*first).to_string();
                    row.conflict_label = action_label(first).to_string();
                }
            }
            row
        })
        .collect()
}

impl AppController {
    pub(super) fn settings_keyboard_bind(&mut self) {
        let state = self.window.global::<SettingsState>();
        state.set_shortcut_rows(self.settings.keyboard.rows.model_rc());
        state.on_shortcut_record(|action| {
            let action = action.to_string();
            crate::ui_thread::with_app(move |app| app.settings_keyboard_record(&action));
        });
        state.on_shortcut_cancel(|| {
            crate::ui_thread::with_app(AppController::settings_keyboard_stop_recording);
        });
        state.on_shortcut_key(|text, primary, shift, alt, secondary| {
            let press = KeyPress {
                text: text.to_string(),
                primary,
                shift,
                alt,
                secondary,
            };
            crate::ui_thread::with_app(move |app| app.settings_keyboard_key(&press));
        });
        state.on_shortcut_reset(|action| {
            let action = action.to_string();
            crate::ui_thread::with_app(move |app| app.settings_keyboard_reset(&action));
        });
        state.on_shortcut_unbind(|action| {
            let action = action.to_string();
            crate::ui_thread::with_app(move |app| app.settings_keyboard_unbind(&action));
        });
        state.on_shortcut_reset_all(|| {
            crate::ui_thread::with_app(AppController::settings_keyboard_reset_all);
        });
        state.on_shortcut_pending_confirm(|| {
            crate::ui_thread::with_app(AppController::settings_keyboard_confirm_pending);
        });
        state.on_shortcut_pending_cancel(|| {
            crate::ui_thread::with_app(|app| {
                app.settings.keyboard.pending = None;
                app.settings_keyboard_refresh();
            });
        });
    }

    /// Whether the Keyboard page is capturing keys for a new binding; the
    /// global shortcut layer must let every key through meanwhile.
    pub(crate) fn settings_is_recording_shortcut(&self) -> bool {
        self.settings.keyboard.recording.is_some()
            && self.settings.page == "keyboard"
            && self.settings_tab_active()
    }

    pub(super) fn settings_keyboard_activate(&mut self) {
        self.settings_keyboard_refresh();
    }

    /// Ends recording, e.g. when the page or tab changes.
    pub(super) fn settings_keyboard_stop_recording(&mut self) {
        if self.settings.keyboard.recording.take().is_some() {
            self.settings_keyboard_refresh();
        }
    }

    fn settings_keyboard_record(&mut self, action: &str) {
        if !crate::shortcuts::is_known(action) {
            return;
        }
        let keyboard = &mut self.settings.keyboard;
        keyboard.recording = Some(action.to_string());
        keyboard.pending = None;
        keyboard.notice = None;
        self.settings_keyboard_refresh();
    }

    fn settings_keyboard_key(&mut self, press: &KeyPress) {
        let Some(action) = self.settings.keyboard.recording.clone() else {
            return;
        };
        match record_press(press) {
            Recorded::Waiting => {}
            Recorded::Cancel => {
                self.settings.keyboard.recording = None;
                self.settings_keyboard_refresh();
            }
            Recorded::Rejected(message) => {
                // Stay in recording mode so the user can try other keys.
                self.settings.keyboard.notice = Some((message, TONE_WARNING));
                self.settings_keyboard_refresh();
            }
            Recorded::Binding(binding) => {
                self.settings.keyboard.recording = None;
                let effective = effective_bindings(&self.prefs.keymap);
                let conflicts: Vec<String> = conflicting_actions(&effective, &action, &binding)
                    .into_iter()
                    .map(str::to_string)
                    .collect();
                if conflicts.is_empty() {
                    self.settings_keyboard_assign(&action, Some(&binding), &[]);
                } else {
                    self.settings.keyboard.pending = Some(PendingBinding {
                        action,
                        binding,
                        conflicts,
                    });
                    self.settings.keyboard.notice = None;
                    self.settings_keyboard_refresh();
                }
            }
        }
    }

    fn settings_keyboard_confirm_pending(&mut self) {
        if let Some(pending) = self.settings.keyboard.pending.take() {
            self.settings_keyboard_assign(
                &pending.action,
                Some(&pending.binding),
                &pending.conflicts,
            );
        }
    }

    fn settings_keyboard_reset(&mut self, action: &str) {
        let default = default_binding(action).and_then(|binding| canonical_binding(binding).ok());
        let effective = effective_bindings(&self.prefs.keymap);
        let conflicts: Vec<String> = default
            .as_deref()
            .map(|binding| conflicting_actions(&effective, action, binding))
            .unwrap_or_default()
            .into_iter()
            .map(str::to_string)
            .collect();
        if conflicts.is_empty() {
            self.prefs.keymap.remove(action);
            self.settings_keyboard_save(Some((
                format!("“{}” is back to its default.", action_label(action)),
                TONE_INFO,
            )));
        } else if let Some(binding) = default {
            // Same flow as recording the default keys by hand.
            self.settings.keyboard.pending = Some(PendingBinding {
                action: action.to_string(),
                binding,
                conflicts,
            });
            self.settings.keyboard.notice = None;
            self.settings_keyboard_refresh();
        }
    }

    fn settings_keyboard_unbind(&mut self, action: &str) {
        if !crate::shortcuts::is_known(action) {
            return;
        }
        set_binding(&mut self.prefs.keymap, action, /*binding*/ None);
        self.settings.keyboard.pending = None;
        self.settings_keyboard_save(Some((
            format!(
                "“{}” no longer has a shortcut. Reset restores the default.",
                action_label(action)
            ),
            TONE_INFO,
        )));
    }

    fn settings_keyboard_reset_all(&mut self) {
        if self.prefs.keymap.is_empty() {
            return;
        }
        self.show_dialog(
            DialogRequest::confirm(
                "Reset all shortcuts?",
                "Every shortcut goes back to its default, and your custom shortcuts are removed from gui.json.",
            )
            .accept_label("Reset all")
            .destructive(),
            Box::new(|app, accepted| {
                if accepted.is_some() {
                    app.prefs.keymap.clear();
                    app.settings.keyboard.pending = None;
                    app.settings.keyboard.recording = None;
                    app.settings_keyboard_save(Some((
                        "All shortcuts are back to their defaults.".to_string(),
                        TONE_SUCCESS,
                    )));
                }
            }),
        );
    }

    /// Binds `action` to `binding`, first unbinding `unbind` (actions that
    /// used the same keys), then saves.
    fn settings_keyboard_assign(&mut self, action: &str, binding: Option<&str>, unbind: &[String]) {
        for other in unbind {
            set_binding(&mut self.prefs.keymap, other, /*binding*/ None);
        }
        set_binding(&mut self.prefs.keymap, action, binding);
        let keys = display_binding(binding.unwrap_or_default());
        let label = action_label(action);
        let message = if unbind.is_empty() {
            format!("“{label}” is now {keys}.")
        } else {
            let names: Vec<String> = unbind
                .iter()
                .map(|other| format!("“{}”", action_label(other)))
                .collect();
            format!(
                "“{label}” is now {keys}. Removed it from {}.",
                names.join(", ")
            )
        };
        self.settings_keyboard_save(Some((message, TONE_SUCCESS)));
    }

    /// Persists `prefs.keymap`, reloads the live keymap, and redraws.
    fn settings_keyboard_save(&mut self, notice: Option<(String, i32)>) {
        self.save_prefs();
        self.reload_keymap();
        self.settings.keyboard.notice = notice;
        self.settings_keyboard_refresh();
        self.settings_diagnostics_refresh();
    }

    pub(super) fn settings_keyboard_refresh(&mut self) {
        let keyboard = &mut self.settings.keyboard;
        let rows = shortcut_rows(&self.prefs.keymap, keyboard.recording.as_deref());
        fn key(row: &ShortcutData) -> &str {
            &row.action
        }
        let mut revision = keyboard.revision;
        keyboard.rows.sync(
            rows,
            key,
            &HashSet::new(),
            &mut revision,
            ShortcutData::to_slint,
        );
        keyboard.revision = revision;

        let state = self.window.global::<SettingsState>();
        state.set_shortcut_recording(keyboard.recording.clone().unwrap_or_default().into());
        state.set_shortcut_any_custom(!self.prefs.keymap.is_empty());
        let (notice, tone) = keyboard.notice.clone().unwrap_or_default();
        state.set_shortcut_notice(notice.into());
        state.set_shortcut_notice_tone(tone);
        let (pending, accept) = match &keyboard.pending {
            Some(pending) => {
                let names: Vec<String> = pending
                    .conflicts
                    .iter()
                    .map(|other| format!("“{}”", action_label(other)))
                    .collect();
                (
                    format!(
                        "{} is already assigned to {}. Reassign it to “{}”?",
                        display_binding(&pending.binding),
                        names.join(", "),
                        action_label(&pending.action)
                    ),
                    "Reassign".to_string(),
                )
            }
            None => (String::new(), String::new()),
        };
        state.set_shortcut_pending(pending.into());
        state.set_shortcut_pending_accept(accept.into());
    }

    /// Actions whose binding differs from the default.
    pub(super) fn settings_keyboard_custom_count(&self) -> usize {
        effective_bindings(&self.prefs.keymap)
            .iter()
            .filter(|entry| entry.custom)
            .count()
    }

    /// Keymap problems for the Diagnostics page.
    pub(super) fn settings_keyboard_problems(&self) -> Vec<String> {
        crate::shortcuts::Keymap::new(&self.prefs.keymap).1
    }

    /// Scripted input for UI automation: `["record", action]`,
    /// `["key", text, "mod,shift,alt,ctrl"]` (straight to the page),
    /// `["press", text, "mod,..."]` (through the window), `["confirm"]`,
    /// `["cancel"]`, `["reset", action]`, `["unbind", action]`.
    pub(super) fn settings_keyboard_automation(&mut self, args: &[String]) {
        let arg = |index: usize| args.get(index).map(String::as_str).unwrap_or_default();
        let state = self.window.global::<SettingsState>();
        match arg(0) {
            "record" => state.invoke_shortcut_record(arg(1).into()),
            "key" => {
                let modifiers: Vec<&str> = arg(2).split(',').collect();
                let has = |name: &str| modifiers.contains(&name);
                let text = match arg(1) {
                    "Escape" => "\u{1b}",
                    "Tab" => "\t",
                    other => other,
                };
                state.invoke_shortcut_key(
                    text.into(),
                    has("mod"),
                    has("shift"),
                    has("alt"),
                    has("ctrl"),
                );
            }
            "press" => {
                // Real key events through the window, so the global shortcut
                // layer and the page's FocusScope both see them. Dispatched
                // after this tick: the shortcut layer needs the controller.
                let text = SharedString::from(match arg(1) {
                    "Escape" => "\u{1b}",
                    "Tab" => "\t",
                    other => other,
                });
                let modifiers: Vec<SharedString> = arg(2)
                    .split(',')
                    .filter_map(|name| match name {
                        "mod" => Some(Key::Control),
                        "shift" => Some(Key::Shift),
                        "alt" => Some(Key::Alt),
                        "ctrl" => Some(Key::Meta),
                        _ => None,
                    })
                    .map(SharedString::from)
                    .collect();
                let window = self.window.as_weak();
                slint::Timer::single_shot(Duration::ZERO, move || {
                    let Some(window) = window.upgrade() else {
                        return;
                    };
                    let window = window.window();
                    for modifier in &modifiers {
                        window.dispatch_event(WindowEvent::KeyPressed {
                            text: modifier.clone(),
                        });
                    }
                    window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                    window.dispatch_event(WindowEvent::KeyReleased { text });
                    for modifier in modifiers.into_iter().rev() {
                        window.dispatch_event(WindowEvent::KeyReleased { text: modifier });
                    }
                });
            }
            "confirm" => state.invoke_shortcut_pending_confirm(),
            "cancel" => state.invoke_shortcut_pending_cancel(),
            "reset" => state.invoke_shortcut_reset(arg(1).into()),
            "unbind" => state.invoke_shortcut_unbind(arg(1).into()),
            other => tracing::warn!(action = other, "unknown keyboard automation action"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn press(text: &str, modifiers: &str) -> KeyPress {
        let has = |name: &str| modifiers.split('+').any(|part| part == name);
        KeyPress {
            text: text.to_string(),
            primary: has("mod"),
            shift: has("shift"),
            alt: has("alt"),
            secondary: has("ctrl"),
        }
    }

    fn binding(text: &str, modifiers: &str) -> Recorded {
        record_press(&press(text, modifiers))
    }

    #[test]
    fn chords_record_as_canonical_bindings() {
        let ok = |text: &str| Recorded::Binding(text.to_string());
        assert_eq!(binding("t", "mod"), ok("Mod+T"));
        assert_eq!(binding("T", "mod+shift"), ok("Mod+Shift+T"));
        assert_eq!(binding("k", "ctrl+alt"), ok("Ctrl+Alt+K"));
        assert_eq!(binding("\t", "mod+shift"), ok("Mod+Shift+Tab"));
        assert_eq!(binding("\u{19}", "mod+shift"), ok("Mod+Shift+Tab"));
        assert_eq!(binding("\u{F700}", "alt"), ok("Alt+Up"));
        assert_eq!(binding("\u{1b}", "mod"), ok("Mod+Escape"));
        assert_eq!(binding(" ", "ctrl"), ok("Ctrl+Space"));
        // Function keys may stand alone.
        assert_eq!(binding("\u{F708}", ""), ok("F5"));
        assert_eq!(binding("\u{F708}", "shift"), ok("Shift+F5"));
        // Shift is part of the symbol's text, so it is left out.
        assert_eq!(binding("!", "mod+shift"), ok("Mod+!"));
        assert_eq!(binding("+", "mod+shift"), ok("Mod++"));
    }

    #[test]
    fn recording_waits_for_modifiers_and_cancels_on_escape() {
        assert_eq!(binding("\u{10}", "shift"), Recorded::Waiting);
        assert_eq!(binding("\u{11}", "mod"), Recorded::Waiting);
        assert_eq!(binding("\u{17}", "ctrl"), Recorded::Waiting);
        assert_eq!(binding("\u{1b}", ""), Recorded::Cancel);
    }

    #[test]
    fn recording_rejects_typing_keys_and_editing_chords() {
        for (text, modifiers) in [("t", ""), ("T", "shift"), ("\n", ""), ("\u{F700}", "")] {
            assert!(
                matches!(binding(text, modifiers), Recorded::Rejected(_)),
                "{text:?} {modifiers}"
            );
        }
        for text in ["c", "v", "x", "a", "z"] {
            assert!(
                matches!(binding(text, "mod"), Recorded::Rejected(_)),
                "{text}"
            );
        }
        assert!(matches!(binding("Z", "mod+shift"), Recorded::Rejected(_)));
        // Unnamed special keys (Insert) have no binding syntax.
        assert!(matches!(binding("\u{F727}", "mod"), Recorded::Rejected(_)));
    }

    #[test]
    fn recorded_bindings_match_the_same_press() {
        for (text, modifiers) in [
            ("n", "mod"),
            ("N", "mod+shift"),
            ("!", "mod+shift"),
            ("\t", "ctrl+shift"),
            ("\u{F70F}", ""),
            ("j", "alt"),
        ] {
            let press = press(text, modifiers);
            let Recorded::Binding(binding) = record_press(&press) else {
                panic!("{text:?} {modifiers} did not record");
            };
            let overrides = BTreeMap::from([("new-tab".to_string(), binding.clone())]);
            let (keymap, errors) = crate::shortcuts::Keymap::new(&overrides);
            assert_eq!(errors, Vec::<String>::new());
            assert_eq!(keymap.lookup(&press), Some("new-tab"), "{binding}");
        }
    }

    #[test]
    fn effective_bindings_apply_overrides() {
        let overrides = BTreeMap::from([
            ("new-tab".to_string(), "cmd+n".to_string()),
            ("close-tab".to_string(), String::new()),
            ("settings".to_string(), "Hyper+,".to_string()),
            ("open-file".to_string(), "Mod+O".to_string()),
        ]);
        let effective = effective_bindings(&overrides);
        let find = |action: &str| {
            effective
                .iter()
                .find(|entry| entry.action == action)
                .cloned()
                .unwrap_or_else(|| panic!("{action} missing"))
        };
        assert_eq!(find("new-tab").binding.as_deref(), Some("Mod+N"));
        assert!(find("new-tab").custom);
        assert_eq!(find("close-tab").binding, None);
        assert!(find("close-tab").custom);
        // Invalid overrides fall back to the default and are reported.
        assert_eq!(find("settings").binding.as_deref(), Some("Mod+,"));
        assert!(find("settings").invalid.is_some());
        // An override equal to the default is not a customization.
        assert!(!find("open-file").custom);
        assert_eq!(effective.len(), actions().count());
    }

    #[test]
    fn conflicts_are_detected_on_canonical_bindings() {
        let overrides = BTreeMap::from([("open-file".to_string(), "mod+t".to_string())]);
        let effective = effective_bindings(&overrides);
        assert_eq!(
            conflicting_actions(&effective, "open-file", "Mod+T"),
            vec!["new-tab"]
        );
        assert_eq!(
            conflicting_actions(&effective, "new-tab", "Mod+T"),
            vec!["open-file"]
        );
        assert_eq!(
            conflicting_actions(&effective, "toggle-info", "Mod+W"),
            vec!["close-tab"]
        );
        assert!(conflicting_actions(&effective, "toggle-info", "Mod+J").is_empty());

        let rows = shortcut_rows(&overrides, /*recording*/ None);
        let new_tab = rows.iter().find(|row| row.action == "new-tab");
        let open_file = rows.iter().find(|row| row.action == "open-file");
        let (Some(new_tab), Some(open_file)) = (new_tab, open_file) else {
            panic!("rows missing");
        };
        assert_eq!(new_tab.conflict_action, "open-file");
        assert_eq!(open_file.conflict_action, "new-tab");
        // "New tab" comes first, so it wins; "Open file" never runs.
        assert!(
            !new_tab.warning.contains("does not run"),
            "{}",
            new_tab.warning
        );
        assert!(
            open_file.warning.contains("does not run"),
            "{}",
            open_file.warning
        );
        assert!(rows.iter().filter(|row| row.warning.is_empty()).count() >= rows.len() - 2);
    }

    #[test]
    fn set_binding_keeps_only_real_customizations() {
        let mut overrides = BTreeMap::new();
        set_binding(&mut overrides, "new-tab", Some("Mod+N"));
        assert_eq!(overrides.get("new-tab").map(String::as_str), Some("Mod+N"));
        set_binding(&mut overrides, "new-tab", Some("Mod+T"));
        assert_eq!(overrides.get("new-tab"), None);
        set_binding(&mut overrides, "close-tab", /*binding*/ None);
        assert_eq!(overrides.get("close-tab").map(String::as_str), Some(""));
    }

    #[test]
    fn rows_show_recording_and_custom_state() {
        let overrides = BTreeMap::from([("toggle-sidebar".to_string(), String::new())]);
        let rows = shortcut_rows(&overrides, Some("new-tab"));
        let sidebar = rows.iter().find(|row| row.action == "toggle-sidebar");
        let new_tab = rows.iter().find(|row| row.action == "new-tab");
        let (Some(sidebar), Some(new_tab)) = (sidebar, new_tab) else {
            panic!("rows missing");
        };
        assert!(sidebar.unbound && sidebar.custom);
        assert_eq!(sidebar.binding, "Unbound");
        assert!(new_tab.recording && !new_tab.custom);
        assert_eq!(new_tab.label, "New tab");
    }
}
