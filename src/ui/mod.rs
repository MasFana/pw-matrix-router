//! Native output matrix and selected-output inspector.

pub mod actions;
mod app_state;

pub use app_state::RouterView;

/// Initialize components, the native dark theme, and desktop commands once.
pub fn init(cx: &mut gpui_kit::App) {
    gpui_kit::init(cx);
    gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
    actions::bind_keys(cx);
}
