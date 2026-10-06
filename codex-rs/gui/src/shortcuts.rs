//! Global keyboard shortcuts and the user-editable keymap.
//!
//! `ui/app.slint` forwards every key press to [`Keymap::lookup`] before the
//! focused widget sees it. Bindings are strings such as `Mod+T`,
//! `Mod+Shift+Tab`, or `Escape`, where `Mod` is Cmd on macOS and Ctrl
//! elsewhere (Slint reports Cmd as `control` on macOS), and `Ctrl` is the
//! Control key on macOS and the Windows/Super key elsewhere. Users override
//! bindings under Help › Keyboard Shortcuts, which writes `gui.json`
//! (`keymap: {"new-tab": "Mod+N"}`); an empty string unbinds an action.

use std::collections::BTreeMap;

/// Next/previous tab: Ctrl+Tab everywhere. macOS reserves Cmd+Tab for the
/// app switcher, so the binding names the Control key there (`Ctrl`), and
/// `Mod` (Ctrl) elsewhere, where `Ctrl` would mean the Windows/Super key.
#[cfg(target_os = "macos")]
const NEXT_TAB: &str = "Ctrl+Tab";
#[cfg(target_os = "macos")]
const PREV_TAB: &str = "Ctrl+Shift+Tab";
#[cfg(not(target_os = "macos"))]
const NEXT_TAB: &str = "Mod+Tab";
#[cfg(not(target_os = "macos"))]
const PREV_TAB: &str = "Mod+Shift+Tab";

/// Actions that can be bound, with their default bindings.
const DEFAULT_BINDINGS: &[(&str, &str)] = &[
    ("new-tab", "Mod+T"),
    ("close-tab", "Mod+W"),
    ("next-tab", NEXT_TAB),
    ("prev-tab", PREV_TAB),
    ("settings", "Mod+,"),
    ("toggle-sidebar", "Mod+B"),
    ("toggle-info", "Mod+Shift+I"),
    ("open-file", "Mod+O"),
    ("escape", "Escape"),
    ("tab-1", "Mod+1"),
    ("tab-2", "Mod+2"),
    ("tab-3", "Mod+3"),
    ("tab-4", "Mod+4"),
    ("tab-5", "Mod+5"),
    ("tab-6", "Mod+6"),
    ("tab-7", "Mod+7"),
    ("tab-8", "Mod+8"),
    ("tab-9", "Mod+9"),
];

/// Human-readable action names for the settings page.
pub(crate) fn action_label(action: &str) -> &'static str {
    match action {
        "new-tab" => "New tab",
        "close-tab" => "Close tab",
        "next-tab" => "Next tab",
        "prev-tab" => "Previous tab",
        "settings" => "Open settings",
        "toggle-sidebar" => "Toggle sidebar",
        "toggle-info" => "Toggle info pane",
        "open-file" => "Open file",
        "escape" => "Interrupt running turn",
        "tab-1" => "Go to tab 1",
        "tab-2" => "Go to tab 2",
        "tab-3" => "Go to tab 3",
        "tab-4" => "Go to tab 4",
        "tab-5" => "Go to tab 5",
        "tab-6" => "Go to tab 6",
        "tab-7" => "Go to tab 7",
        "tab-8" => "Go to tab 8",
        "tab-9" => "Go to last tab",
        _ => "Unknown action",
    }
}

/// Every bindable action, in display order.
pub(crate) fn actions() -> impl Iterator<Item = &'static str> {
    DEFAULT_BINDINGS.iter().map(|(action, _)| *action)
}

/// Default binding of `action`, if it is a known action.
pub(crate) fn default_binding(action: &str) -> Option<&'static str> {
    DEFAULT_BINDINGS
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, binding)| *binding)
}

/// Whether `name` is a shortcut action the app handles.
pub(crate) fn is_known(name: &str) -> bool {
    default_binding(name).is_some()
}

/// A key press as reported by Slint.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct KeyPress {
    pub(crate) text: String,
    /// Cmd on macOS, Ctrl elsewhere.
    pub(crate) primary: bool,
    pub(crate) shift: bool,
    pub(crate) alt: bool,
    /// Ctrl on macOS, the Windows/Super key elsewhere.
    pub(crate) secondary: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Chord {
    key: String,
    primary: bool,
    shift: bool,
    alt: bool,
    secondary: bool,
}

impl Chord {
    fn parse(binding: &str) -> Result<Self, String> {
        let mut chord = Chord {
            key: String::new(),
            primary: false,
            shift: false,
            alt: false,
            secondary: false,
        };
        let parts: Vec<&str> = binding.split('+').map(str::trim).collect();
        // "Mod++" binds the plus key.
        let (modifiers, key) = match parts.as_slice() {
            [.., "", ""] => (&parts[..parts.len() - 2], "+"),
            [modifiers @ .., key] => (modifiers, *key),
            [] => return Err("empty binding".to_string()),
        };
        for modifier in modifiers {
            match modifier.to_ascii_lowercase().as_str() {
                "mod" | "cmd" | "primary" => chord.primary = true,
                "shift" => chord.shift = true,
                "alt" | "option" => chord.alt = true,
                "ctrl" | "control" | "meta" | "super" | "win" => chord.secondary = true,
                other => return Err(format!("unknown modifier `{other}`")),
            }
        }
        chord.key = normalize_key(key).ok_or_else(|| format!("unknown key `{key}`"))?;
        Ok(chord)
    }

    /// Canonical binding text, e.g. `Mod+Shift+T`.
    fn to_binding(&self) -> String {
        let mut parts = Vec::new();
        for (on, name) in [
            (self.primary, "Mod"),
            (self.secondary, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
        ] {
            if on {
                parts.push(name);
            }
        }
        parts.push(&self.key);
        parts.join("+")
    }

    fn matches(&self, press: &KeyPress) -> bool {
        let Some(key) = key_from_text(&press.text) else {
            return false;
        };
        // Shift changes the produced text for symbols, so only require the
        // flags the binding names, plus an exact match on the rest.
        key == self.key
            && press.primary == self.primary
            && press.alt == self.alt
            && press.secondary == self.secondary
            && (press.shift == self.shift || (!self.shift && is_symbol(&self.key)))
    }
}

fn is_symbol(key: &str) -> bool {
    key.chars().count() == 1 && key.chars().all(|c| !c.is_alphanumeric())
}

/// Canonical key name for a binding token.
fn normalize_key(token: &str) -> Option<String> {
    let named = match token.to_ascii_lowercase().as_str() {
        "tab" => "Tab",
        "escape" | "esc" => "Escape",
        "enter" | "return" => "Return",
        "space" => "Space",
        "backspace" => "Backspace",
        "delete" | "del" => "Delete",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        lower if lower.len() >= 2 && lower.starts_with('f') && lower[1..].parse::<u8>().is_ok() => {
            return Some(token.to_ascii_uppercase());
        }
        _ => {
            let mut chars = token.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            return Some(first.to_uppercase().collect());
        }
    };
    Some(named.to_string())
}

/// Canonical key name for the text Slint reports for a key press.
fn key_from_text(text: &str) -> Option<String> {
    let mut chars = text.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    // Slint encodes special keys as private-use code points (see Key in
    // i-slint-common); map the ones we support.
    let named = match first {
        '\t' => "Tab",
        '\u{19}' => "Tab", // Backtab (Shift+Tab)
        '\u{1b}' => "Escape",
        '\n' | '\r' => "Return",
        ' ' => "Space",
        '\u{8}' => "Backspace",
        '\u{7f}' => "Delete",
        '\u{F700}' => "Up",
        '\u{F701}' => "Down",
        '\u{F702}' => "Left",
        '\u{F703}' => "Right",
        '\u{F729}' => "Home",
        '\u{F72B}' => "End",
        '\u{F72C}' => "PageUp",
        '\u{F72D}' => "PageDown",
        c @ '\u{F704}'..='\u{F71B}' => {
            return Some(format!("F{}", u32::from(c) - 0xF704 + 1));
        }
        other => return Some(other.to_uppercase().collect()),
    };
    Some(named.to_string())
}

/// Canonical form of `binding`, so equal chords compare equal
/// (`cmd+shift+t` → `Mod+Shift+T`).
pub(crate) fn canonical_binding(binding: &str) -> Result<String, String> {
    Chord::parse(binding).map(|chord| chord.to_binding())
}

/// Binding text for a recorded key press, or `None` for a bare modifier or
/// a key that cannot be bound. Shift is left out for symbols, whose text
/// already reflects it (Shift+1 records `!`).
pub(crate) fn binding_for_press(press: &KeyPress) -> Option<String> {
    let key = key_from_text(&press.text)?;
    let unbindable = |c: char| {
        // Modifier keys alone, other control codes, and special keys Slint
        // reports as private-use code points that have no binding name.
        c.is_control() || ('\u{E000}'..='\u{F8FF}').contains(&c)
    };
    if key.chars().count() == 1 && key.chars().all(unbindable) {
        return None;
    }
    let chord = Chord {
        shift: press.shift && !is_symbol(&key),
        key,
        primary: press.primary,
        alt: press.alt,
        secondary: press.secondary,
    };
    Some(chord.to_binding())
}

/// Resolved bindings: defaults with user overrides applied.
#[derive(Clone, Debug)]
pub(crate) struct Keymap {
    bindings: Vec<(String, Chord)>,
}

impl Keymap {
    /// Builds the keymap. Invalid overrides are reported and ignored.
    pub(crate) fn new(overrides: &BTreeMap<String, String>) -> (Self, Vec<String>) {
        let mut errors = Vec::new();
        let mut bindings = Vec::new();
        for (action, default) in DEFAULT_BINDINGS {
            let binding = overrides
                .get(*action)
                .map(String::as_str)
                .unwrap_or(default);
            if binding.trim().is_empty() {
                continue;
            }
            match Chord::parse(binding) {
                Ok(chord) => bindings.push(((*action).to_string(), chord)),
                Err(err) => {
                    errors.push(format!("keymap: {action} = \"{binding}\": {err}"));
                    if let Ok(chord) = Chord::parse(default) {
                        bindings.push(((*action).to_string(), chord));
                    }
                }
            }
        }
        for action in overrides.keys() {
            if !is_known(action) {
                errors.push(format!("keymap: unknown action `{action}`"));
            }
        }
        (Self { bindings }, errors)
    }

    /// Action bound to `press`, if any.
    pub(crate) fn lookup(&self, press: &KeyPress) -> Option<&str> {
        self.bindings
            .iter()
            .find(|(_, chord)| chord.matches(press))
            .map(|(action, _)| action.as_str())
    }
}

/// Platform-specific rendering of a binding (`Mod` → ⌘ on macOS).
pub(crate) fn display_binding(binding: &str) -> String {
    if binding.trim().is_empty() {
        return "Unbound".to_string();
    }
    let primary = if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl"
    };
    let secondary = if cfg!(target_os = "macos") {
        "⌃"
    } else {
        "Win"
    };
    binding
        .split('+')
        .map(|part| match part.trim().to_ascii_lowercase().as_str() {
            "mod" | "cmd" | "primary" => primary.to_string(),
            "ctrl" | "control" | "meta" | "super" | "win" => secondary.to_string(),
            "alt" | "option" => if cfg!(target_os = "macos") {
                "⌥"
            } else {
                "Alt"
            }
            .to_string(),
            "shift" => if cfg!(target_os = "macos") {
                "⇧"
            } else {
                "Shift"
            }
            .to_string(),
            _ => part.trim().to_string(),
        })
        .collect::<Vec<_>>()
        .join(if cfg!(target_os = "macos") { "" } else { "+" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn press(text: &str, primary: bool, shift: bool) -> KeyPress {
        KeyPress {
            text: text.to_string(),
            primary,
            shift,
            ..KeyPress::default()
        }
    }

    /// A press of the Control key chord used for tab cycling: `secondary`
    /// (the Control key) on macOS, `primary` (Ctrl) elsewhere.
    fn ctrl_press(text: &str, shift: bool) -> KeyPress {
        KeyPress {
            text: text.to_string(),
            primary: !cfg!(target_os = "macos"),
            secondary: cfg!(target_os = "macos"),
            shift,
            ..KeyPress::default()
        }
    }

    #[test]
    fn default_bindings_resolve() {
        let (keymap, errors) = Keymap::new(&BTreeMap::new());
        assert!(errors.is_empty());
        assert_eq!(keymap.lookup(&press("t", true, false)), Some("new-tab"));
        assert_eq!(keymap.lookup(&press("T", true, false)), Some("new-tab"));
        assert_eq!(keymap.lookup(&ctrl_press("\t", false)), Some("next-tab"));
        assert_eq!(keymap.lookup(&ctrl_press("\t", true)), Some("prev-tab"));
        assert_eq!(keymap.lookup(&ctrl_press("\u{19}", true)), Some("prev-tab"));
        assert_eq!(keymap.lookup(&press("I", true, true)), Some("toggle-info"));
        assert_eq!(keymap.lookup(&press(",", true, false)), Some("settings"));
        assert_eq!(
            keymap.lookup(&press("\u{1b}", false, false)),
            Some("escape")
        );
        assert_eq!(keymap.lookup(&press("3", true, false)), Some("tab-3"));
        assert_eq!(keymap.lookup(&press("t", false, false)), None);
        assert_eq!(keymap.lookup(&press("i", true, false)), None);
    }

    #[test]
    fn overrides_replace_and_unbind() {
        let overrides = BTreeMap::from([
            ("new-tab".to_string(), "Mod+N".to_string()),
            ("close-tab".to_string(), String::new()),
        ]);
        let (keymap, errors) = Keymap::new(&overrides);
        assert!(errors.is_empty());
        assert_eq!(keymap.lookup(&press("n", true, false)), Some("new-tab"));
        assert_eq!(keymap.lookup(&press("t", true, false)), None);
        assert_eq!(keymap.lookup(&press("w", true, false)), None);
    }

    #[test]
    fn invalid_overrides_fall_back_and_report() {
        let overrides = BTreeMap::from([
            ("new-tab".to_string(), "Hyper+T".to_string()),
            ("bogus".to_string(), "Mod+X".to_string()),
        ]);
        let (keymap, errors) = Keymap::new(&overrides);
        assert_eq!(errors.len(), 2);
        assert_eq!(keymap.lookup(&press("t", true, false)), Some("new-tab"));
    }

    #[test]
    fn parses_function_keys_and_plus() -> Result<(), String> {
        assert_eq!(Chord::parse("F5")?.key, "F5");
        assert_eq!(Chord::parse("Mod++")?.key, "+");
        assert!(Chord::parse("Mod+Nope").is_err());
        let f5 = Chord::parse("F5")?;
        assert!(f5.matches(&press("\u{F708}", false, false)));
        Ok(())
    }

    #[test]
    fn tab_cycling_never_defaults_to_the_macos_app_switcher() {
        let (keymap, _) = Keymap::new(&BTreeMap::new());
        let cmd_tab = press("\t", true, false);
        if cfg!(target_os = "macos") {
            // Cmd+Tab never reaches the window on macOS.
            assert_eq!(keymap.lookup(&cmd_tab), None);
            assert_eq!(display_binding(NEXT_TAB), "⌃Tab");
            assert_eq!(display_binding(PREV_TAB), "⌃⇧Tab");
        } else {
            assert_eq!(keymap.lookup(&cmd_tab), Some("next-tab"));
            assert_eq!(display_binding(NEXT_TAB), "Ctrl+Tab");
        }
    }

    #[test]
    fn every_action_has_a_label() {
        for action in actions() {
            assert_ne!(action_label(action), "Unknown action", "{action}");
        }
    }
}
