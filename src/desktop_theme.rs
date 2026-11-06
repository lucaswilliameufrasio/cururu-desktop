//! Shared native component theme; semantic diff colors remain independent.

use gpui::{App, px, rgb};
use gpui_component::{Theme, ThemeMode};

pub const CODE_FONT: &str = if cfg!(target_os = "macos") {
    "Menlo"
} else {
    "DejaVu Sans Mono"
};

pub fn init(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let theme = Theme::global_mut(cx);
    theme.background = rgb(0x0018_222d).into();
    theme.foreground = rgb(0x00e6_edf3).into();
    theme.border = rgb(0x0035_4758).into();
    theme.input = theme.border;
    theme.primary = rgb(0x0079_b8ff).into();
    theme.primary_foreground = rgb(0x0011_1820).into();
    theme.primary_hover = rgb(0x00a4_cfff).into();
    theme.primary_active = rgb(0x005c_a5f5).into();
    theme.secondary = rgb(0x0023_3241).into();
    theme.secondary_foreground = theme.foreground;
    theme.secondary_hover = rgb(0x002f_4357).into();
    theme.secondary_active = theme.secondary_hover;
    theme.muted = rgb(0x0020_2b36).into();
    theme.muted_foreground = rgb(0x00a6_b5c5).into();
    theme.accent = theme.secondary;
    theme.accent_foreground = theme.foreground;
    theme.ring = theme.primary;
    theme.selection = theme.secondary_hover;
    theme.radius = px(6.0);
    theme.radius_lg = px(10.0);
    theme.shadow = false;
}
