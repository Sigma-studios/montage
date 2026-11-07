//! The editor's look: dark and quiet, after Génie's palette.

use eframe::egui::{self, Color32, CornerRadius, Stroke, Vec2};

pub const WINDOW: Color32 = Color32::from_rgb(0x20, 0x23, 0x26);
pub const PANEL: Color32 = Color32::from_rgb(0x1a, 0x1c, 0x1f);
pub const INPUT: Color32 = Color32::from_rgb(0x14, 0x16, 0x18);
pub const BORDER: Color32 = Color32::from_rgb(0x43, 0x48, 0x4d);
pub const SELECTION: Color32 = Color32::from_rgb(0x1f, 0x4e, 0x68);
pub const TEXT: Color32 = Color32::from_rgb(0xfc, 0xfc, 0xfc);
pub const SUBTEXT: Color32 = Color32::from_rgb(0xa1, 0xa9, 0xb1);
/// The cut handles and the caption being edited.
pub const ACCENT: Color32 = Color32::from_rgb(0xf2, 0xb3, 0x3d);
pub const WAVEFORM: Color32 = Color32::from_rgb(0x6f, 0xb6, 0xd9);
pub const PREVIEW_BG: Color32 = Color32::from_rgb(0x0b, 0x0c, 0x0d);
pub const ERROR: Color32 = Color32::from_rgb(0xf0, 0x6a, 0x6a);
pub const WARN: Color32 = Color32::from_rgb(0xf2, 0xc0, 0x5c);

/// Applies the editor's style to `style` (the editor's own `Ui`, so the
/// other views keep theirs).
pub fn apply(style: &mut egui::Style) {
    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.panel_fill = WINDOW;
    v.window_fill = WINDOW;
    v.extreme_bg_color = INPUT;
    v.faint_bg_color = PANEL;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.selection.bg_fill = SELECTION;
    v.selection.stroke = Stroke::new(1.0, TEXT);
    v.warn_fg_color = WARN;
    v.error_fg_color = ERROR;
    v.slider_trailing_fill = true;

    let radius = CornerRadius::same(6);
    let w = &mut v.widgets;
    w.noninteractive.fg_stroke = Stroke::new(1.0, SUBTEXT);
    w.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x2e, 0x32, 0x36));
    for (state, fill) in [
        (&mut w.inactive, Color32::from_rgb(0x2b, 0x2f, 0x33)),
        (&mut w.hovered, Color32::from_rgb(0x35, 0x3a, 0x3f)),
        (&mut w.active, Color32::from_rgb(0x3d, 0x43, 0x49)),
        (&mut w.open, Color32::from_rgb(0x35, 0x3a, 0x3f)),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.corner_radius = radius;
        state.expansion = 0.0;
    }
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.inactive.bg_stroke = Stroke::NONE;
    w.hovered.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.corner_radius = radius;

    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(8.0, 8.0);
    s.button_padding = Vec2::new(10.0, 5.0);
    s.interact_size.y = 26.0;
    s.slider_width = 160.0;
}

/// A section title: small, light grey.
pub fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(SUBTEXT).size(12.5).strong());
}

/// A button that shows whether it's on, framed either way so it reads as a
/// button.
pub fn choice(
    ui: &mut egui::Ui,
    selected: bool,
    text: impl Into<egui::WidgetText>,
) -> egui::Response {
    ui.add(egui::Button::new(text.into()).selected(selected))
}

/// A framed on/off toggle.
pub fn toggle(ui: &mut egui::Ui, on: &mut bool, text: &str) -> egui::Response {
    let mut response = choice(ui, *on, text);
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response
}
