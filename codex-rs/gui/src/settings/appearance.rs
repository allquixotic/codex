//! Settings › Appearance: GUI-only preferences stored in `gui.json`.

use slint::ComponentHandle;

use crate::app::AppController;
use crate::prefs::BusyInput;
use crate::prefs::RendererChoice;
use crate::prefs::ThemeChoice;
use crate::ui::AppState;
use crate::ui::SettingsState;

const MIN_FONT_SIZE: i32 = 9;
const MAX_FONT_SIZE: i32 = 24;

pub(crate) fn theme_index(theme: ThemeChoice) -> i32 {
    match theme {
        ThemeChoice::System => 0,
        ThemeChoice::Light => 1,
        ThemeChoice::Dark => 2,
    }
}

pub(crate) fn theme_from_index(index: i32) -> ThemeChoice {
    match index {
        1 => ThemeChoice::Light,
        2 => ThemeChoice::Dark,
        _ => ThemeChoice::System,
    }
}

pub(crate) fn renderer_index(renderer: RendererChoice) -> i32 {
    match renderer {
        RendererChoice::Auto => 0,
        RendererChoice::Software => 1,
        RendererChoice::Gpu => 2,
    }
}

pub(crate) fn renderer_from_index(index: i32) -> RendererChoice {
    match index {
        1 => RendererChoice::Software,
        2 => RendererChoice::Gpu,
        _ => RendererChoice::Auto,
    }
}

pub(crate) fn renderer_label(renderer: RendererChoice) -> &'static str {
    match renderer {
        RendererChoice::Auto => "Auto",
        RendererChoice::Software => "Software",
        RendererChoice::Gpu => "GPU",
    }
}

impl AppController {
    pub(super) fn settings_appearance_bind(&mut self) {
        self.window
            .global::<SettingsState>()
            .on_pref_changed(|name| {
                let name = name.to_string();
                crate::ui_thread::with_app(move |app| app.settings_pref_changed(&name));
            });
    }

    /// Pushes the current preferences into the page.
    pub(super) fn settings_appearance_show(&mut self) {
        let prefs = &self.prefs;
        let state = self.window.global::<SettingsState>();
        let app_state = self.window.global::<AppState>();
        state.set_pref_theme(theme_index(prefs.theme));
        state.set_pref_font_size(prefs.font_size.round() as i32);
        state.set_pref_renderer(renderer_index(prefs.renderer));
        state.set_pref_enter_sends(prefs.enter_sends);
        state.set_pref_busy_input(i32::from(prefs.busy_input == BusyInput::Queue));
        state.set_pref_notifications(prefs.desktop_notifications);
        state.set_pref_cross_tab(prefs.cross_tab_tools);
        state.set_pref_sidebar(app_state.get_sidebar_visible());
        state.set_pref_info(app_state.get_info_visible());
        state.set_renderer_in_use(self.settings.renderer_in_use.as_str().into());
    }

    fn settings_pref_changed(&mut self, name: &str) {
        let state = self.window.global::<SettingsState>();
        match name {
            "theme" => {
                self.prefs.theme = theme_from_index(state.get_pref_theme());
                self.apply_theme();
            }
            "font-size" => {
                let size = state
                    .get_pref_font_size()
                    .clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
                state.set_pref_font_size(size);
                self.prefs.font_size = size as f32;
                self.apply_theme();
            }
            "renderer" => {
                let renderer = renderer_from_index(state.get_pref_renderer());
                if renderer != self.prefs.renderer {
                    self.prefs.renderer = renderer;
                    self.toast(format!(
                        "Renderer set to {}. Restart Codex to apply it.",
                        renderer_label(renderer)
                    ));
                }
            }
            "enter-sends" => self.prefs.enter_sends = state.get_pref_enter_sends(),
            "busy-input" => {
                self.prefs.busy_input = if state.get_pref_busy_input() == 1 {
                    BusyInput::Queue
                } else {
                    BusyInput::Steer
                };
            }
            "notifications" => self.prefs.desktop_notifications = state.get_pref_notifications(),
            "cross-tab" => self.prefs.cross_tab_tools = state.get_pref_cross_tab(),
            "sidebar" => {
                let visible = state.get_pref_sidebar();
                self.prefs.sidebar_visible = visible;
                self.window
                    .global::<AppState>()
                    .set_sidebar_visible(visible);
            }
            "info" => {
                let visible = state.get_pref_info();
                self.prefs.info_pane_visible = visible;
                self.window.global::<AppState>().set_info_visible(visible);
            }
            other => {
                tracing::warn!(name = other, "unknown preference");
                return;
            }
        }
        self.save_prefs();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn indexes_round_trip() {
        for theme in [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark] {
            assert_eq!(theme_from_index(theme_index(theme)), theme);
        }
        for renderer in [
            RendererChoice::Auto,
            RendererChoice::Software,
            RendererChoice::Gpu,
        ] {
            assert_eq!(renderer_from_index(renderer_index(renderer)), renderer);
        }
        assert_eq!(theme_from_index(-1), ThemeChoice::System);
        assert_eq!(renderer_from_index(7), RendererChoice::Auto);
        assert_eq!(renderer_label(RendererChoice::Gpu), "GPU");
    }
}
