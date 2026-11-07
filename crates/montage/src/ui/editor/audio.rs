//! One row per audio track: its name, a mute toggle and a volume slider
//! from 0 to 200 %. Changes reach the player live (the view compares the
//! gains every frame) and are saved with the edit.

use eframe::egui::{self, RichText};
use montage_core::Config;
use montage_core::edit::{Edit, TrackSettings};

use super::style;

/// The gain of each of the clip's `tracks` audio tracks.
pub fn gains(edit: &Edit, tracks: usize) -> Vec<f32> {
    (0..tracks).map(|i| edit.track(i).gain()).collect()
}

pub fn show(ui: &mut egui::Ui, edit: &mut Edit, tracks: usize, config: &Config) {
    style::heading(ui, "Audio");
    if tracks == 0 {
        ui.label(RichText::new("This clip has no audio.").color(style::SUBTEXT));
        return;
    }
    egui::Grid::new("montage-audio")
        .num_columns(3)
        .spacing([14.0, 6.0])
        .show(ui, |ui| {
            for index in 0..tracks {
                let mut track = edit.track(index);
                let before = track;
                ui.add_sized(
                    [90.0, 22.0],
                    egui::Label::new(RichText::new(config.track_name(index)).color(style::TEXT))
                        .truncate(),
                );
                let mute = egui::Button::new("Mute")
                    .selected(track.muted)
                    .min_size([64.0, 0.0].into());
                if ui.add(mute).on_hover_text("Silence this track").clicked() {
                    track.muted = !track.muted;
                }
                ui.add_enabled_ui(!track.muted, |ui| {
                    let mut percent = (track.volume * 100.0).round();
                    let slider = egui::Slider::new(&mut percent, 0.0..=200.0)
                        .suffix(" %")
                        .fixed_decimals(0)
                        .step_by(1.0);
                    let response = ui.add(slider);
                    if response.changed() {
                        track.volume = percent / 100.0;
                    }
                    if response.double_clicked() {
                        track.volume = 1.0;
                    }
                    response.on_hover_text("Double-click for 100 %");
                });
                if track != before {
                    edit.set_track(TrackSettings { index, ..track });
                }
                ui.end_row();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gains_follow_mute_and_volume() {
        let mut edit = Edit::default();
        edit.set_track(TrackSettings {
            index: 1,
            muted: true,
            volume: 1.5,
        });
        edit.set_track(TrackSettings {
            index: 0,
            muted: false,
            volume: 1.5,
        });
        assert_eq!(gains(&edit, 3), vec![1.5, 0.0, 1.0]);
        assert_eq!(gains(&edit, 0), Vec::<f32>::new());
    }
}
