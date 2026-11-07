//! The captions panel: "Add text", the list of captions, and the settings
//! of the one selected: text, font, size, colours, outline, capitals,
//! shadow, and timing.

use std::sync::{Arc, Mutex};

use eframe::egui::{self, RichText};
use montage_core::edit::{Caption, Color, Cut, DEFAULT_FONT};
use montage_text::Fonts;

use super::font_picker::{self, FontPicker};
use super::style;
use super::timeline::{MIN_CUT, caption_range, format_time};

/// A new caption: the classic look, at the top for the first one and at the
/// bottom for the next.
pub fn new_caption(existing: &[Caption]) -> Caption {
    let mut caption = Caption {
        text: "Your text".into(),
        ..Caption::default()
    };
    if existing.iter().any(|c| (c.y - caption.y).abs() < 0.05) {
        caption.y = 1.0 - caption.y;
    }
    caption
}

/// A short name for a caption in the list.
pub fn caption_label(caption: &Caption) -> String {
    let text = caption.text.trim();
    if text.is_empty() {
        return "(empty)".into();
    }
    let mut label: String = text.chars().take(18).collect();
    if text.chars().count() > 18 {
        label.push('…');
    }
    label
}

/// A colour picker for `color`'s red, green and blue; its alpha is kept.
fn color_button(ui: &mut egui::Ui, color: &mut Color) -> egui::Response {
    let mut rgb = [color.r, color.g, color.b];
    let response = egui::color_picker::color_edit_button_srgb(ui, &mut rgb);
    if response.changed() {
        [color.r, color.g, color.b] = rgb;
    }
    response
}

pub struct CaptionsInput<'a> {
    pub position: f64,
    pub cut: Cut,
    pub duration: f64,
    pub fonts: Option<&'a Arc<Mutex<Fonts>>>,
}

#[derive(Default)]
pub struct CaptionsPanel {
    pub picker: FontPicker,
    focus_text: bool,
}

impl CaptionsPanel {
    /// Returns true when the recent fonts changed and the config should be
    /// saved.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        input: CaptionsInput,
        captions: &mut Vec<Caption>,
        selected: &mut Option<usize>,
        recent_fonts: &mut Vec<String>,
    ) -> bool {
        let mut recent_changed = false;
        ui.horizontal(|ui| {
            style::heading(ui, "Text");
            if ui.button("+ Add text").clicked() {
                captions.push(new_caption(captions));
                *selected = Some(captions.len() - 1);
                self.focus_text = true;
            }
            for (i, caption) in captions.iter().enumerate() {
                if style::choice(ui, *selected == Some(i), caption_label(caption)).clicked() {
                    *selected = if *selected == Some(i) { None } else { Some(i) };
                }
            }
            if captions.is_empty() {
                ui.label(
                    RichText::new("Captions show over the video; drag them into place.")
                        .color(style::SUBTEXT),
                );
            }
        });

        let Some(index) = selected.filter(|&i| i < captions.len()) else {
            *selected = None;
            return false;
        };
        let mut delete = false;
        let caption = &mut captions[index];

        ui.horizontal(|ui| {
            let text = ui.add(
                egui::TextEdit::singleline(&mut caption.text)
                    .hint_text("Caption text")
                    .desired_width(240.0),
            );
            if std::mem::take(&mut self.focus_text) {
                text.request_focus();
            }

            match input.fonts {
                Some(fonts) => {
                    let button = ui
                        .add(egui::Button::new(caption.font.as_str()).min_size([150.0, 0.0].into()))
                        .on_hover_text("Font");
                    let just_opened = button.clicked();
                    let popup = egui::Popup::from_toggle_button_response(&button)
                        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                        .width(300.0);
                    popup.show(|ui| {
                        let mut fonts = fonts.lock().unwrap();
                        let picked = self.picker.show(
                            ui,
                            &mut *fonts,
                            recent_fonts,
                            &caption.font,
                            just_opened,
                        );
                        if let Some(family) = picked {
                            caption.font = family.clone();
                            let before = recent_fonts.clone();
                            font_picker::push_recent(recent_fonts, &family);
                            recent_changed = *recent_fonts != before;
                            ui.close();
                        }
                    });
                }
                None => {
                    ui.add_enabled(false, egui::Button::new(caption.font.as_str()));
                    ui.spinner();
                    ui.label(RichText::new("Loading fonts…").color(style::SUBTEXT));
                }
            }

            ui.label("Size");
            let mut percent = caption.size * 100.0;
            if ui
                .add(
                    egui::Slider::new(&mut percent, 2.0..=30.0)
                        .suffix(" %")
                        .fixed_decimals(1),
                )
                .on_hover_text("Line height, as a share of the video's height")
                .changed()
            {
                caption.size = percent / 100.0;
            }

            if ui.button("Delete").clicked() {
                delete = true;
            }
        });

        ui.horizontal(|ui| {
            ui.label("Fill");
            color_button(ui, &mut caption.fill).on_hover_text("Fill colour");
            ui.label("Outline");
            color_button(ui, &mut caption.outline).on_hover_text("Outline colour");
            let mut width = caption.outline_width * 100.0;
            if ui
                .add(
                    egui::Slider::new(&mut width, 0.0..=25.0)
                        .suffix(" %")
                        .fixed_decimals(0),
                )
                .on_hover_text("Outline width, as a share of the text's size")
                .changed()
            {
                caption.outline_width = width / 100.0;
            }
            style::toggle(ui, &mut caption.uppercase, "Capitals");
            style::toggle(ui, &mut caption.shadow, "Shadow");

            ui.separator();
            timing(ui, caption, &input);
        });

        if let Some(fonts) = input.fonts
            && caption.font != DEFAULT_FONT
            && !fonts.lock().unwrap().has_family(&caption.font)
        {
            ui.label(
                RichText::new(format!(
                    "“{}” isn't installed on this computer: {DEFAULT_FONT} is used instead.",
                    caption.font
                ))
                .color(style::WARN),
            );
        }

        if delete {
            captions.remove(index);
            *selected = None;
        }
        recent_changed
    }
}

/// When the caption shows: the whole cut by default, or from and to times
/// set at the playhead (or dragged on the timeline).
fn timing(ui: &mut egui::Ui, caption: &mut Caption, input: &CaptionsInput) {
    let (start, end) = caption_range(caption, input.cut);
    let shown = if caption.start.is_none() && caption.end.is_none() {
        "Whole cut".to_string()
    } else {
        format!("{} – {}", format_time(start), format_time(end))
    };
    ui.label("Shows");
    ui.label(RichText::new(shown).color(style::TEXT).monospace());
    let t = input.position;
    if ui
        .button("From here")
        .on_hover_text("Start the caption at the playhead")
        .clicked()
    {
        caption.start = Some(t.min(end - MIN_CUT).max(0.0));
    }
    if ui
        .button("Until here")
        .on_hover_text("End the caption at the playhead")
        .clicked()
    {
        caption.end = Some(t.max(start + MIN_CUT).min(input.duration));
    }
    if (caption.start.is_some() || caption.end.is_some()) && ui.button("Whole cut").clicked() {
        caption.start = None;
        caption.end = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_captions_take_the_free_edge() {
        let first = new_caption(&[]);
        assert_eq!(first.font, DEFAULT_FONT);
        assert!(first.uppercase);
        assert_eq!(first.y, Caption::default().y);
        let second = new_caption(std::slice::from_ref(&first));
        assert!((second.y - (1.0 - first.y)).abs() < 1e-6);
    }

    #[test]
    fn labels_are_short() {
        let mut c = Caption::default();
        assert_eq!(caption_label(&c), "(empty)");
        c.text = "gg".into();
        assert_eq!(caption_label(&c), "gg");
        c.text = "a very long caption about that shot".into();
        assert_eq!(caption_label(&c), "a very long captio…");
    }
}
