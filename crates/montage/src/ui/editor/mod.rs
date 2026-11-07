//! The editor: preview, timeline (filmstrip, waveform, cut handles), one row
//! per audio track, captions, and the export bar.
//!
//! The edit is saved to the clip's `montage.toml` half a second after it
//! last changed, and when the editor is left.

mod audio;
mod captions;
mod debounce;
mod export;
mod font_picker;
mod keys;
mod playback;
mod preview;
mod style;
mod thumbs;
mod timeline;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Layout, Margin, RichText};
use montage_core::edit::Cut;
use montage_core::{Clip, Edit};
use montage_media::MediaInfo;

use self::captions::{CaptionsInput, CaptionsPanel};
use self::debounce::Debounce;
use self::export::{EstimateFn, ExportBar, ExportContext};
use self::keys::{Action, Shuttle};
use self::playback::PlayerSlot;
use self::preview::{Preview, PreviewInput};
use self::thumbs::{Loadable, Thumbs};
use self::timeline::{DragTarget, TimelineInput, WaveInput, format_time};
use super::{Nav, Shared};

const SAVE_DELAY: Duration = Duration::from_millis(500);
/// The player is told the new preview size once the window stops changing.
const RESIZE_DELAY: Duration = Duration::from_millis(200);

pub struct EditorView {
    path: PathBuf,
    /// Boxed: the editor is large, and the app keeps views in an enum.
    state: Result<Box<Editor>, String>,
}

impl EditorView {
    pub fn new(ctx: &egui::Context, shared: &mut Shared, path: PathBuf) -> EditorView {
        let state = Editor::open(ctx, shared, &path).map(Box::new);
        EditorView { path, state }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared) -> Option<Nav> {
        style::apply(ui.style_mut());
        match &mut self.state {
            Ok(editor) => editor.ui(ui, shared),
            Err(error) => error_screen(ui, &self.path, error),
        }
    }
}

impl Drop for EditorView {
    /// Closing the window mid-change still saves it.
    fn drop(&mut self) {
        if let Ok(editor) = &mut self.state {
            editor.save_now();
        }
    }
}

fn error_screen(ui: &mut egui::Ui, path: &Path, error: &str) -> Option<Nav> {
    let mut nav = None;
    egui::CentralPanel::default().show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() / 3.0);
            ui.label(
                RichText::new("Can't open this clip")
                    .size(20.0)
                    .color(style::TEXT),
            );
            ui.add_space(6.0);
            ui.label(RichText::new(error).color(style::ERROR));
            ui.label(RichText::new(path.display().to_string()).color(style::SUBTEXT));
            ui.add_space(14.0);
            if ui.button("Back to the library").clicked() {
                nav = Some(Nav::Library);
            }
        });
    });
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        nav = Some(Nav::Library);
    }
    nav
}

/// The cut as the edit stores it: none when it's the whole clip.
fn store_cut(edit: &mut Edit, cut: Cut, duration: f64) {
    let whole = cut.start <= 1e-3 && (duration - cut.end).abs() <= 1e-3;
    edit.cut = if whole { None } else { Some(cut) };
}

/// The status line's description of the clip.
fn clip_summary(info: &MediaInfo, cut: Cut) -> String {
    format!(
        "{}×{} · {} fps · {} · cut {}",
        info.width,
        info.height,
        trim_fps(info.fps),
        format_time(info.duration),
        format_time(cut.duration()),
    )
}

/// A triangle, or two bars while playing: drawn, since egui's fonts have
/// no such symbols.
fn play_icon(ui: &egui::Ui, rect: egui::Rect, playing: bool, enabled: bool) {
    use egui::{Pos2, Shape, Stroke, vec2};
    let color = if enabled {
        style::TEXT
    } else {
        style::SUBTEXT.gamma_multiply(0.5)
    };
    let c = rect.center();
    let painter = ui.painter();
    if playing {
        for dx in [-3.5, 3.5] {
            let bar = egui::Rect::from_center_size(c + vec2(dx, 0.0), vec2(4.0, 12.0));
            painter.rect_filled(bar, 1.0, color);
        }
    } else {
        let points = vec![
            Pos2::new(c.x - 4.0, c.y - 6.5),
            Pos2::new(c.x + 6.5, c.y),
            Pos2::new(c.x - 4.0, c.y + 6.5),
        ];
        painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
    }
}

fn trim_fps(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.01 {
        format!("{:.0}", fps)
    } else {
        format!("{:.2}", fps)
    }
}

fn export_context<'a>(
    clip: &'a Clip,
    info: &'a MediaInfo,
    edit: &'a Edit,
    shared: &'a Shared,
) -> ExportContext<'a> {
    ExportContext {
        clip,
        info,
        config: &shared.config,
        cut: edit.cut_within(info.duration),
        gains: audio::gains(edit, info.audio_tracks),
        captions: &edit.captions,
        fonts: shared.fonts.get(),
    }
}

struct Editor {
    clip: Clip,
    info: MediaInfo,
    edit: Edit,
    /// The edit as last seen, to notice changes.
    seen: Edit,
    /// The edit as last written.
    saved: Edit,
    save: Debounce,
    save_error: Option<String>,
    config_error: Option<String>,

    player: PlayerSlot,
    /// Whether a pending player is opened (not in tests).
    open_player: bool,
    position: f64,
    shuttle: Shuttle,
    looping: bool,
    sent_gains: Option<Vec<f32>>,
    sent_loop: Option<Option<(f64, f64)>>,
    /// The size the player decodes to, and the one it will be told once
    /// the window settles.
    requested_px: [u32; 2],
    resize_target: [u32; 2],
    resize: Debounce,

    thumbs: Thumbs,
    preview: Preview,
    timeline_drag: Option<DragTarget>,
    selected: Option<usize>,
    captions: CaptionsPanel,
    export: ExportBar,
}

impl Editor {
    fn open(ctx: &egui::Context, shared: &Shared, path: &Path) -> Result<Editor, String> {
        let clip = Clip::locate(path)
            .and_then(|clip| clip.adopt())
            .map_err(|e| e.to_string())?;
        // An edit that can't be read is never overwritten.
        let edit = Edit::load(&clip.edit_path())
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        let info = montage_media::probe(&clip.original).map_err(|e| e.to_string())?;
        let thumbs = Thumbs::start(ctx, clip.original.clone(), clip.thumbs_dir());
        let mut editor = Editor::from_parts(
            shared,
            clip,
            edit,
            info,
            PlayerSlot::Pending,
            thumbs,
            montage_media::estimate_bytes,
        );
        editor.open_player = true;
        Ok(editor)
    }

    fn from_parts(
        shared: &Shared,
        clip: Clip,
        edit: Edit,
        info: MediaInfo,
        player: PlayerSlot,
        thumbs: Thumbs,
        estimate: EstimateFn,
    ) -> Editor {
        let position = edit.cut_within(info.duration).start;
        Editor {
            clip,
            info,
            seen: edit.clone(),
            saved: edit.clone(),
            edit,
            save: Debounce::new(SAVE_DELAY),
            save_error: None,
            config_error: None,
            player,
            open_player: false,
            position,
            shuttle: Shuttle::default(),
            looping: false,
            sent_gains: None,
            sent_loop: None,
            requested_px: [0, 0],
            resize_target: [0, 0],
            resize: Debounce::new(RESIZE_DELAY),
            thumbs,
            preview: Preview::default(),
            timeline_drag: None,
            selected: None,
            captions: CaptionsPanel::default(),
            export: ExportBar::new(&shared.config, estimate),
        }
    }

    fn cut(&self) -> Cut {
        self.edit.cut_within(self.info.duration)
    }

    fn save_now(&mut self) {
        self.save.cancel();
        if self.edit == self.saved {
            return;
        }
        match self.edit.save(&self.clip.edit_path()) {
            Ok(()) => {
                self.saved = self.edit.clone();
                self.save_error = None;
            }
            Err(e) => self.save_error = Some(e.to_string()),
        }
    }

    /// The shortcuts pressed this frame, taken out of the input so widgets
    /// don't also react (Space on a focused button).
    fn read_keys(&self, ui: &mut egui::Ui) -> Vec<Action> {
        let ctx = ui.ctx();
        if ctx.egui_wants_keyboard_input() || egui::Popup::is_any_open(ctx) {
            return Vec::new();
        }
        let mut actions = Vec::new();
        ui.input_mut(|i| {
            i.events.retain(|event| {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    repeat,
                    modifiers,
                    ..
                } = event
                    && let Some(action) = keys::action_for(*key, *modifiers, *repeat)
                {
                    actions.push(action);
                    return false;
                }
                true
            });
        });
        actions
    }

    fn apply(&mut self, action: Action, ctx: &egui::Context, shared: &Shared) -> Option<Nav> {
        let duration = self.info.duration;
        let cut = self.cut();
        match action {
            Action::PlayPause => self.toggle_play(),
            Action::Step(frames) => {
                self.shuttle = Shuttle::default();
                if let Some(player) = self.player.get() {
                    player.step(frames);
                }
            }
            Action::ShuttleBack | Action::ShuttlePause | Action::ShuttleForward => {
                let speed = self.shuttle.press(action);
                if let Some(player) = self.player.get() {
                    if speed == 0.0 {
                        player.pause();
                    } else {
                        player.set_speed(speed);
                        player.play();
                    }
                }
            }
            Action::SetCutStart => {
                store_cut(
                    &mut self.edit,
                    timeline::set_in(cut, self.position, duration),
                    duration,
                );
            }
            Action::SetCutEnd => {
                store_cut(
                    &mut self.edit,
                    timeline::set_out(cut, self.position, duration),
                    duration,
                );
            }
            Action::Back => {
                self.save_now();
                return Some(Nav::Library);
            }
            Action::Save => self.save_now(),
            Action::Export => {
                let cx = export_context(&self.clip, &self.info, &self.edit, shared);
                self.export.start(ctx, &cx);
            }
        }
        None
    }

    fn toggle_play(&mut self) {
        let cut = self.cut();
        let position = self.position;
        let Some(player) = self.player.get() else {
            return;
        };
        if player.is_playing() {
            player.pause();
            self.shuttle = Shuttle::default();
        } else {
            // From the end of the cut, play it again from its start.
            if position >= cut.end - 0.02 {
                player.seek(cut.start);
                self.position = cut.start;
            }
            player.set_speed(1.0);
            player.play();
            self.shuttle = Shuttle { speed: 1.0 };
        }
    }

    fn seek(&mut self, t: f64) {
        self.position = t;
        if let Some(player) = self.player.get() {
            player.seek(t);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared) -> Option<Nav> {
        let ctx = ui.ctx().clone();
        let now = Instant::now();
        self.thumbs.poll(&ctx);
        self.export.poll();
        if let Some(frame) = self.player.get().and_then(|p| p.take_frame()) {
            self.preview.upload(&ctx, frame);
        }

        let mut nav = None;
        for action in self.read_keys(ui) {
            if let Some(n) = self.apply(action, &ctx, shared) {
                nav = Some(n);
            }
        }
        if let Some(player) = self.player.get_ref() {
            self.position = player.position();
        }
        let playing = self.player.get_ref().is_some_and(|p| p.is_playing());
        let fonts = shared.fonts.get();
        let duration = self.info.duration;
        let cut_before = self.cut();
        let mut cut = cut_before;

        let bar = |top: i8, bottom: i8| {
            egui::Frame::new().fill(style::WINDOW).inner_margin(Margin {
                left: 16,
                right: 16,
                top,
                bottom,
            })
        };

        egui::Panel::bottom("montage-editor-status")
            .frame(
                egui::Frame::new()
                    .fill(style::PANEL)
                    .inner_margin(Margin::symmetric(16, 5)),
            )
            .show_separator_line(false)
            .resizable(false)
            .show(ui, |ui| self.status_line(ui));

        egui::Panel::bottom("montage-editor-export")
            .frame(bar(10, 10))
            .resizable(false)
            .show(ui, |ui| {
                let cx = export_context(&self.clip, &self.info, &self.edit, shared);
                self.export.show(ui, &cx);
            });

        egui::Panel::bottom("montage-editor-captions")
            .frame(bar(8, 10))
            .show_separator_line(false)
            .resizable(false)
            .show(ui, |ui| {
                let input = CaptionsInput {
                    position: self.position,
                    cut,
                    duration,
                    fonts: fonts.as_ref(),
                };
                let recent_changed = self.captions.show(
                    ui,
                    input,
                    &mut self.edit.captions,
                    &mut self.selected,
                    &mut shared.config.editor.recent_fonts,
                );
                if recent_changed {
                    self.config_error = shared.save_config().err().map(|e| format!("{e:#}"));
                }
            });

        egui::Panel::bottom("montage-editor-audio")
            .frame(bar(8, 4))
            .show_separator_line(false)
            .resizable(false)
            .show(ui, |ui| {
                audio::show(ui, &mut self.edit, self.info.audio_tracks, &shared.config);
            });

        egui::Panel::bottom("montage-editor-timeline")
            .frame(bar(8, 6))
            .show_separator_line(false)
            .resizable(false)
            .show(ui, |ui| {
                self.transport(ui, playing, &mut cut);
                let gains = audio::gains(&self.edit, self.info.audio_tracks);
                self.thumbs.mix(&gains);
                let waveform = match self.thumbs.mixed() {
                    Some((peaks, bucket_seconds, scale)) => WaveInput::Ready {
                        peaks,
                        bucket_seconds,
                        scale,
                    },
                    None => match &self.thumbs.waveform {
                        Loadable::Failed(e) => WaveInput::Failed(e),
                        _ => WaveInput::Loading,
                    },
                };
                let input = TimelineInput {
                    duration,
                    position: self.position,
                    filmstrip: &self.thumbs.filmstrip,
                    waveform,
                    selected_caption: self.selected,
                };
                let out = timeline::show(
                    ui,
                    &mut self.timeline_drag,
                    input,
                    &mut cut,
                    &mut self.edit.captions,
                );
                if let Some(i) = out.select_caption {
                    self.selected = Some(i);
                }
                if let Some(t) = out.seek {
                    self.seek(t);
                }
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(style::WINDOW).inner_margin(Margin {
                left: 16,
                right: 16,
                top: 14,
                bottom: 4,
            }))
            .show(ui, |ui| {
                let message = match &self.player {
                    PlayerSlot::Pending => Some(("Opening…".to_string(), style::SUBTEXT)),
                    PlayerSlot::Failed(e) => {
                        Some((format!("Can't play this clip: {e}"), style::ERROR))
                    }
                    PlayerSlot::Open(_) => None,
                };
                let input = PreviewInput {
                    video_size: [self.info.width, self.info.height],
                    position: self.position,
                    cut,
                    fonts: fonts.as_ref(),
                    selected: self.selected,
                    message,
                };
                let out = self.preview.show(ui, input, &mut self.edit.captions);
                if out.toggle_play {
                    self.toggle_play();
                }
                if let Some(i) = out.select {
                    self.selected = Some(i);
                }
                self.size_player(out.display_px, now);
            });

        if cut != cut_before {
            store_cut(&mut self.edit, cut, duration);
        }
        self.sync_player();

        // Save once the changes settle.
        if self.edit != self.seen {
            self.seen = self.edit.clone();
            self.save.poke(now);
        }
        if self.save.fire(now) {
            self.save_now();
        }
        if let Some(left) = self.save.remaining(now) {
            ctx.request_repaint_after(left);
        }
        if let Some(left) = self.resize.remaining(now) {
            ctx.request_repaint_after(left);
        }
        if playing {
            ctx.request_repaint();
        } else if self.export.is_running() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        nav
    }

    /// Opens the player at the preview's size on the first frame, then
    /// follows the window's size.
    fn size_player(&mut self, display_px: [u32; 2], now: Instant) {
        let [w, h] = display_px;
        if matches!(self.player, PlayerSlot::Pending) && self.open_player {
            self.player = match montage_media::Player::open(&self.clip.original, w, h) {
                Ok(player) => {
                    let mut player: Box<dyn playback::Playback> = Box::new(player);
                    if self.position > 0.0 {
                        player.seek(self.position);
                    }
                    PlayerSlot::Open(player)
                }
                Err(e) => PlayerSlot::Failed(e.to_string()),
            };
            self.requested_px = display_px;
            self.resize_target = display_px;
            self.sent_gains = None;
            self.sent_loop = None;
            return;
        }
        if display_px == self.requested_px {
            self.resize.cancel();
            self.resize_target = display_px;
        } else if display_px != self.resize_target {
            self.resize_target = display_px;
            self.resize.poke(now);
        }
        if self.resize.fire(now) {
            self.requested_px = self.resize_target;
            let [w, h] = self.resize_target;
            if let Some(player) = self.player.get() {
                player.set_preview_size(w, h);
            }
        }
    }

    /// Gives the player the gains and loop the edit asks for, when they
    /// changed.
    fn sync_player(&mut self) {
        let cut = self.cut();
        let gains = audio::gains(&self.edit, self.info.audio_tracks);
        let range = self.looping.then_some((cut.start, cut.end));
        let Some(player) = self.player.get() else {
            return;
        };
        if self.sent_gains.as_ref() != Some(&gains) {
            player.set_gains(&gains);
            self.sent_gains = Some(gains);
        }
        if self.sent_loop != Some(range) {
            player.set_loop(range);
            self.sent_loop = Some(range);
        }
    }

    /// Play/pause, the time, the loop toggle and the cut's length.
    fn transport(&mut self, ui: &mut egui::Ui, playing: bool, cut: &mut Cut) {
        ui.horizontal(|ui| {
            let hint = if playing {
                "Pause (Space)"
            } else {
                "Play (Space)"
            };
            let play = egui::Button::new("").min_size([38.0, 26.0].into());
            let response = ui
                .add_enabled(self.player.get_ref().is_some(), play)
                .accessible_name(hint)
                .on_hover_text(hint);
            play_icon(ui, response.rect, playing, response.enabled());
            if response.clicked() {
                self.toggle_play();
            }
            ui.label(
                RichText::new(format!(
                    "{} / {}",
                    format_time(self.position),
                    format_time(self.info.duration)
                ))
                .monospace()
                .color(style::TEXT),
            );
            style::toggle(ui, &mut self.looping, "Loop").on_hover_text("Replay just the cut");

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let whole = cut.start <= 1e-3 && (self.info.duration - cut.end).abs() <= 1e-3;
                if ui
                    .add_enabled(!whole, egui::Button::new("Reset cut"))
                    .clicked()
                {
                    *cut = Cut {
                        start: 0.0,
                        end: self.info.duration,
                    };
                }
                ui.label(
                    RichText::new(format!(
                        "Cut {} – {}  ({:.2} s)",
                        format_time(cut.start),
                        format_time(cut.end),
                        cut.duration()
                    ))
                    .monospace()
                    .color(style::SUBTEXT),
                )
                .on_hover_text("I and O set the start and end at the playhead");
            });
        });
        ui.add_space(4.0);
    }

    fn status_line(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let decoder = match &self.player {
                PlayerSlot::Open(player)
                    if !player.has_audio_output() && self.info.audio_tracks > 0 =>
                {
                    format!("{} · no audio output", player.decoder_name())
                }
                PlayerSlot::Open(player) => player.decoder_name(),
                PlayerSlot::Pending => "opening".into(),
                PlayerSlot::Failed(_) => "no preview".into(),
            };
            let small = |text: String| RichText::new(text).size(12.0).color(style::SUBTEXT);
            ui.label(small(format!(
                "{decoder} · {}",
                clip_summary(&self.info, self.cut())
            )));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (text, color) = if let Some(e) = &self.save_error {
                    (format!("Not saved: {e}"), style::ERROR)
                } else if let Some(e) = &self.config_error {
                    (format!("Settings not saved: {e}"), style::ERROR)
                } else if let Some(status) = self.export.status() {
                    status
                } else if self.edit != self.saved {
                    ("Saving…".into(), style::SUBTEXT)
                } else {
                    (
                        self.clip.name().to_string_lossy().into_owned(),
                        style::SUBTEXT,
                    )
                };
                ui.label(RichText::new(text).size(12.0).color(color));
            });
        });
    }
}

#[cfg(test)]
mod tests;
