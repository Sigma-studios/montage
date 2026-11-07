//! The settings page: capture, replay buffer, audio tracks, folders, sharing
//! and sounds. Saving writes `config.toml` and restarts the recorder service
//! (Linux).
//!
//! The page edits a copy of the config; nothing changes until Save. On
//! Linux it asks gpu-screen-recorder (and Wayland) on a thread what can be
//! captured, so it offers only what works, while keeping the current values
//! and free text for when the lists can't be had.

mod apply;
mod probe;
#[cfg(target_os = "linux")]
mod wayland;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use eframe::egui::{self, Color32, RichText};
use montage_core::Config;
use montage_core::recorder::{Codec, ReplayStorage};

use super::library::format_bytes;
use super::{Nav, Shared};
use apply::{FolderCheck, Progress};
use probe::Probe;

const CONTENT_WIDTH: f32 = 820.0;
const MIB: f64 = 1024.0 * 1024.0;

enum Dialog {
    /// Saving restarts the recorder: confirm, maybe after the recording.
    Restart { recording: bool },
    /// Leaving with unsaved changes.
    Discard,
}

enum Status {
    Ok(String),
    Working(String),
    Error(String),
}

pub struct SettingsView {
    // Boxed, as two configs would make this view much larger than the
    // others in the window's screen enum.
    draft: Box<Config>,
    /// The config as last saved (or as found when the page opened).
    saved: Box<Config>,
    /// Whether the recorder's sections show: on Linux, where Montage runs
    /// gpu-screen-recorder.
    recorder: bool,
    probing: Option<Receiver<Probe>>,
    lists: Option<Probe>,
    folder_checks: HashMap<PathBuf, FolderCheck>,
    dialog: Option<Dialog>,
    restart: Option<Receiver<Progress>>,
    status: Option<Status>,
}

/// What a row of the audio list asked for.
enum TrackAction {
    Up(usize),
    Down(usize),
    Remove(usize),
    Add,
}

impl SettingsView {
    pub fn new(ctx: &egui::Context, shared: &mut Shared) -> SettingsView {
        SettingsView::with_options(ctx, shared, cfg!(target_os = "linux"), None)
    }

    /// `lists` given: used as is, nothing is run (tests).
    fn with_options(
        ctx: &egui::Context,
        shared: &mut Shared,
        recorder: bool,
        lists: Option<Probe>,
    ) -> SettingsView {
        let probing = (recorder && lists.is_none()).then(|| {
            let (tx, rx) = mpsc::channel();
            let ctx = ctx.clone();
            let program = crate::controls::Programs::from_env().recorder;
            std::thread::spawn(move || {
                let _ = tx.send(probe::run(&program));
                ctx.request_repaint();
            });
            rx
        });
        SettingsView {
            draft: Box::new(shared.config.clone()),
            saved: Box::new(shared.config.clone()),
            recorder,
            probing,
            lists,
            folder_checks: HashMap::new(),
            dialog: None,
            restart: None,
            status: None,
        }
    }

    fn dirty(&self) -> bool {
        self.draft != self.saved
    }

    /// Whether saving the draft restarts the recorder.
    fn needs_restart(&self) -> bool {
        self.recorder && self.draft.recorder != self.saved.recorder
    }

    fn receive(&mut self) {
        if let Some(rx) = &self.probing
            && let Ok(lists) = rx.try_recv()
        {
            self.lists = Some(lists);
            self.probing = None;
        }
        if let Some(rx) = &self.restart {
            for progress in rx.try_iter() {
                self.status = Some(match progress {
                    Progress::Waiting => Status::Working(
                        "Saved. The recorder restarts with the new settings when the recording \
                         stops; keep Montage open until then."
                            .into(),
                    ),
                    Progress::Restarting => {
                        Status::Working("Saved. Restarting the recorder…".into())
                    }
                    Progress::Done(Ok(())) => Status::Ok(
                        "Saved. The recorder restarted with the new settings; the replay buffer \
                         starts over."
                            .into(),
                    ),
                    Progress::Done(Err(e)) => {
                        Status::Error(format!("Saved, but the recorder didn't restart: {e}"))
                    }
                });
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared) -> Option<Nav> {
        self.receive();
        let ctx = ui.ctx().clone();
        let mut nav = None;

        // Escape closes an open list first, then leaves.
        if self.dialog.is_none()
            && !egui::Popup::is_any_open(&ctx)
            && ui.input(|i| i.key_pressed(egui::Key::Escape))
        {
            nav = self.leave();
        }

        let problems = self.problems();

        egui::Panel::top("settings-header").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Back to the library").clicked() {
                    nav = self.leave();
                }
                ui.heading("Settings");
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("settings-footer").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let save_label = if self.needs_restart() {
                    "Save and restart the recorder"
                } else {
                    "Save"
                };
                let can_save = self.dirty() && problems.is_empty();
                if ui
                    .add_enabled(can_save, egui::Button::new(save_label))
                    .clicked()
                {
                    self.ask_save(shared, &ctx);
                }
                if ui.button("Cancel").clicked() {
                    nav = self.leave();
                }
                ui.add_space(8.0);
                if let Some(problem) = problems.first() {
                    ui.colored_label(ui.visuals().error_fg_color, problem);
                } else if let Some(status) = &self.status {
                    match status {
                        Status::Ok(text) => {
                            ui.colored_label(Color32::from_rgb(70, 170, 90), text);
                        }
                        Status::Working(text) => {
                            ui.spinner();
                            ui.label(text);
                        }
                        Status::Error(text) => {
                            ui.colored_label(ui.visuals().error_fg_color, text);
                        }
                    }
                } else if self.dirty() {
                    ui.weak("Unsaved changes");
                }
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let width = ui.available_width().min(CONTENT_WIDTH);
                    let margin = ((ui.available_width() - width) / 2.0).max(0.0);
                    ui.horizontal(|ui| {
                        ui.add_space(margin);
                        ui.vertical(|ui| {
                            ui.set_width(width);
                            self.sections(ui);
                            ui.add_space(16.0);
                        });
                    });
                });
        });

        if let Some(n) = self.dialogs(&ctx, shared) {
            nav = Some(n);
        }
        nav
    }

    /// Back to the library, asking first when there are unsaved changes.
    fn leave(&mut self) -> Option<Nav> {
        if self.dirty() {
            self.dialog = Some(Dialog::Discard);
            None
        } else {
            Some(Nav::Library)
        }
    }

    /// What keeps the draft from being saved, worded for the footer.
    fn problems(&mut self) -> Vec<String> {
        let mut problems = Vec::new();
        let d = &self.draft;
        if self.recorder {
            let r = &d.recorder;
            if r.monitor.trim().is_empty() {
                problems.push("Choose a screen to capture.".to_string());
            } else if let Some(Ok(monitors)) = self.lists.as_ref().map(|l| &l.monitors)
                && !monitors.is_empty()
                && r.monitor != "screen"
                && r.monitor != self.saved.recorder.monitor
                && !monitors.iter().any(|m| m.name == r.monitor)
            {
                problems.push(format!("No screen named {} is connected.", r.monitor));
            }
            if let Some(i) = r.audio.iter().position(|a| a.trim().is_empty()) {
                problems.push(format!("Audio track {} has no source.", i + 1));
            }
            for (what, folder) in [("replays", &r.replay_dir), ("recordings", &r.record_dir)] {
                let check = self
                    .folder_checks
                    .entry(folder.clone())
                    .or_insert_with(|| apply::check_folder(folder));
                if let FolderCheck::Problem(p) = check {
                    problems.push(format!("The {what} folder: {p}."));
                }
            }
        }
        let s = &d.share;
        if s.min_video_kbps > s.max_video_kbps {
            problems
                .push("Sharing: the lowest video bitrate is above the highest one.".to_string());
        }
        if s.max_bytes == 0 {
            problems.push("Sharing: the size limit can't be zero.".to_string());
        }
        if d.library.as_ref().is_some_and(|l| l.is_empty()) {
            problems.push("Add a library folder, or use the default ones.".to_string());
        }
        problems
    }

    fn ask_save(&mut self, shared: &mut Shared, ctx: &egui::Context) {
        if self.needs_restart() {
            self.dialog = Some(Dialog::Restart {
                recording: apply::recording_running(),
            });
        } else if self.save(shared) {
            self.status = Some(Status::Ok(if self.recorder {
                "Saved. The recorder's own settings didn't change, so it keeps running.".into()
            } else {
                "Saved.".into()
            }));
            ctx.request_repaint();
        }
    }

    /// Writes the draft to `config.toml`. False (with the error shown) when
    /// that failed, leaving the running config as it was.
    fn save(&mut self, shared: &mut Shared) -> bool {
        let mut config = (*self.draft).clone();
        // What the editor remembers may have moved on meanwhile.
        config.editor = shared.config.editor.clone();
        let previous = std::mem::replace(&mut shared.config, config.clone());
        match shared.save_config() {
            Ok(()) => {
                *self.draft = config.clone();
                *self.saved = config;
                self.folder_checks.clear();
                true
            }
            Err(e) => {
                shared.config = previous;
                self.status = Some(Status::Error(format!("Can't save the settings: {e:#}")));
                false
            }
        }
    }

    fn apply(&mut self, shared: &mut Shared, ctx: &egui::Context, after_recording: bool) {
        if !self.save(shared) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let flag = if after_recording {
            apply::recording_flag()
        } else {
            None
        };
        let wake = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        apply::restart_later(flag, apply::restart_service, tx, wake);
        self.restart = Some(rx);
        self.status = Some(Status::Working("Saved. Restarting the recorder…".into()));
    }

    fn dialogs(&mut self, ctx: &egui::Context, shared: &mut Shared) -> Option<Nav> {
        let dialog = self.dialog.as_ref()?;
        let mut nav = None;
        let mut close = false;
        let mut action: Option<bool> = None; // Some(after_recording)
        let replay = self.saved.recorder.replay_seconds;
        let response = egui::Modal::new(egui::Id::unique("settings-dialog")).show(ctx, |ui| {
            ui.set_max_width(480.0);
            match dialog {
                Dialog::Restart { recording } => {
                    ui.heading("Restart the recorder?");
                    ui.add_space(6.0);
                    ui.label(format!(
                        "The new settings take effect when the recorder restarts. Restarting \
                         empties the replay buffer: the last {replay} seconds can't be saved \
                         any more."
                    ));
                    if *recording {
                        ui.add_space(6.0);
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            "A recording is running. Restarting now ends it.",
                        );
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if *recording {
                            if ui.button("Apply after the recording stops").clicked() {
                                action = Some(true);
                            }
                            if ui.button("Apply now").clicked() {
                                action = Some(false);
                            }
                        } else if ui.button("Save and restart").clicked() {
                            action = Some(false);
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Discard => {
                    ui.heading("Discard your changes?");
                    ui.add_space(6.0);
                    ui.label("The settings you changed haven't been saved.");
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Discard").clicked() {
                            nav = Some(Nav::Library);
                        }
                        if ui.button("Keep editing").clicked() {
                            close = true;
                        }
                    });
                }
            }
        });
        if response.should_close() {
            close = true;
        }
        if let Some(after_recording) = action {
            self.dialog = None;
            self.apply(shared, ctx, after_recording);
        } else if close || nav.is_some() {
            self.dialog = None;
        }
        nav
    }

    fn sections(&mut self, ui: &mut egui::Ui) {
        let SettingsView {
            draft,
            saved,
            recorder,
            probing,
            lists,
            folder_checks,
            ..
        } = self;
        let lists = lists.as_ref();

        if *recorder {
            if probing.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak("Looking up screens and audio sources…");
                });
            }
            section(ui, "Capture", |ui| capture(ui, draft, saved, lists));
            section(ui, "Replay buffer", |ui| replay(ui, draft));
            section(ui, "Audio", |ui| audio(ui, draft, lists));
            section(ui, "Folders", |ui| folders(ui, draft, folder_checks));
        }
        section(ui, "Sharing", |ui| sharing(ui, draft));
        section(ui, "GIF", |ui| gif(ui, draft));
        if *recorder {
            section(ui, "Sounds", |ui| sounds(ui, draft));
        }
        section(ui, "Library folders", |ui| {
            library_folders(ui, draft, *recorder)
        });
    }
}

fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(12.0);
    ui.label(RichText::new(title).size(16.0).strong());
    ui.add_space(4.0);
    egui::Frame::group(ui.style())
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn grid(id: &str) -> egui::Grid {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([18.0, 10.0])
        .min_col_width(130.0)
}

fn hint(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).weak().small());
}

fn capture(ui: &mut egui::Ui, draft: &mut Config, saved: &Config, lists: Option<&Probe>) {
    grid("capture").show(ui, |ui| {
        let label = ui.label("Screen").id;
        ui.vertical(|ui| {
            screen_picker(
                ui,
                label,
                &mut draft.recorder.monitor,
                &saved.recorder.monitor,
                lists,
            );
            match lists.map(|l| &l.monitors) {
                Some(Err(e)) => hint(
                    ui,
                    format!("Can't list the screens ({e}). Type a connector name such as DP-1."),
                ),
                Some(Ok(m)) if m.is_empty() => hint(
                    ui,
                    "gpu-screen-recorder lists no screens. Type a connector name such as DP-1.",
                ),
                _ => {}
            }
        });
        ui.end_row();

        let r = &mut draft.recorder;
        ui.label("Frame rate");
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut r.fps)
                    .range(1..=500)
                    .suffix(" fps"),
            );
            for fps in [30, 60, 120, 144] {
                if ui.selectable_label(r.fps == fps, fps.to_string()).clicked() {
                    r.fps = fps;
                }
            }
        });
        ui.end_row();

        ui.label("Codec");
        ui.horizontal(|ui| {
            ui.radio_value(&mut r.codec, Codec::Hevc, "HEVC (H.265)");
            ui.radio_value(&mut r.codec, Codec::H264, "H.264");
            ui.radio_value(&mut r.codec, Codec::Av1, "AV1");
        });
        ui.end_row();

        ui.label("Quality");
        ui.horizontal(|ui| {
            let mut mbps = f64::from(r.bitrate_kbps) / 1000.0;
            let drag = egui::DragValue::new(&mut mbps)
                .range(1.0..=500.0)
                .speed(0.5)
                .max_decimals(1)
                .suffix(" Mbit/s");
            if ui.add(drag).changed() {
                r.bitrate_kbps = (mbps * 1000.0).round() as u32;
            }
            let per_minute = f64::from(r.bitrate_kbps) * 60.0 / 8.0 * 1000.0;
            ui.label(format!("≈ {} per minute", format_bytes(per_minute as u64)));
        });
        ui.end_row();
    });
}

fn screen_picker(
    ui: &mut egui::Ui,
    label: egui::Id,
    monitor: &mut String,
    saved: &str,
    lists: Option<&Probe>,
) {
    let listed = match lists.map(|l| &l.monitors) {
        Some(Ok(monitors)) if !monitors.is_empty() => monitors,
        _ => {
            ui.add(
                egui::TextEdit::singleline(monitor)
                    .desired_width(360.0)
                    .hint_text("DP-1, HDMI-A-1, or screen for the first one"),
            )
            .labelled_by(label);
            return;
        }
    };
    let mut options: Vec<String> = vec!["screen".into()];
    options.extend(listed.iter().map(|m| m.name.clone()));
    for keep in [saved, monitor.as_str()] {
        if !keep.is_empty() && !options.iter().any(|o| o == keep) {
            options.push(keep.to_string());
        }
    }
    egui::ComboBox::from_id_salt("screen")
        .width(420.0)
        .selected_text(screen_label(monitor, lists))
        .show_ui(ui, |ui| {
            for option in options {
                let label = screen_label(&option, lists);
                ui.selectable_value(monitor, option, label);
            }
        })
        .response
        .labelled_by(label);
}

/// `DP-5 — Dell U2720Q · 2560x1440`.
fn screen_label(value: &str, lists: Option<&Probe>) -> String {
    if value == "screen" {
        return "The first screen (screen)".into();
    }
    let mut label = value.to_string();
    let Some(lists) = lists else {
        return label;
    };
    if let Some(output) = lists.outputs.iter().find(|o| o.name == value)
        && !output.model.is_empty()
    {
        label.push_str(&format!(" — {}", output.model));
    }
    if let Ok(monitors) = &lists.monitors {
        match monitors.iter().find(|m| m.name == value) {
            Some(m) => {
                if let Some(size) = &m.size {
                    label.push_str(&format!(" · {size}"));
                }
            }
            None if !monitors.is_empty() => label.push_str(" (not connected)"),
            None => {}
        }
    }
    label
}

fn replay(ui: &mut egui::Ui, draft: &mut Config) {
    let r = &mut draft.recorder;
    grid("replay").show(ui, |ui| {
        ui.label("Length");
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut r.replay_seconds)
                    .range(5..=3600)
                    .suffix(" s"),
            );
            let mb = r.replay_megabytes();
            let place = match r.replay_storage {
                ReplayStorage::Ram => "of memory",
                ReplayStorage::Disk => "of disk space",
            };
            ui.label(format!(
                "≈ {} {place} at this quality",
                format_bytes((mb * 1e6) as u64)
            ));
        });
        ui.end_row();

        ui.label("Kept in");
        ui.horizontal(|ui| {
            ui.radio_value(&mut r.replay_storage, ReplayStorage::Ram, "Memory");
            ui.radio_value(&mut r.replay_storage, ReplayStorage::Disk, "Disk");
        });
        ui.end_row();
    });
    hint(
        ui,
        "This is also the longest a saved replay can be. Changing it restarts the recorder, which \
         empties the buffer.",
    );
}

fn audio(ui: &mut egui::Ui, draft: &mut Config, lists: Option<&Probe>) {
    let mut action = None;
    let count = draft.recorder.audio.len();
    if count == 0 {
        hint(ui, "No audio: the clips will be silent.");
    }
    egui::Grid::new("audio-tracks")
        .num_columns(4)
        .spacing([10.0, 8.0])
        .show(ui, |ui| {
            let (name_label, source_label_id, raw_label) = if count > 0 {
                let ids = (
                    ui.weak("Name").id,
                    ui.weak("Source").id,
                    ui.weak("As gpu-screen-recorder's -a").id,
                );
                ui.label("");
                ui.end_row();
                ids
            } else {
                (egui::Id::NULL, egui::Id::NULL, egui::Id::NULL)
            };
            for i in 0..count {
                let mut name = draft.track_name(i);
                if ui
                    .add_sized([110.0, 20.0], egui::TextEdit::singleline(&mut name))
                    .labelled_by(name_label)
                    .changed()
                {
                    pad_track_names(draft);
                    draft.track_names[i] = name;
                }
                let source = &mut draft.recorder.audio[i];
                source_picker(ui, i, source_label_id, source, lists);
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [200.0, 20.0],
                        egui::TextEdit::singleline(source).hint_text("default_output"),
                    )
                    .labelled_by(raw_label);
                    if !source_known(source, lists) {
                        ui.colored_label(ui.visuals().warn_fg_color, "?")
                            .on_hover_text(
                                "Not in gpu-screen-recorder's lists right now (an app that isn't \
                                 playing, or a device that isn't plugged in).",
                            );
                    }
                });
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(i > 0, egui::Button::new("Up").small())
                        .on_hover_text("Move up")
                        .clicked()
                    {
                        action = Some(TrackAction::Up(i));
                    }
                    if ui
                        .add_enabled(i + 1 < count, egui::Button::new("Down").small())
                        .on_hover_text("Move down")
                        .clicked()
                    {
                        action = Some(TrackAction::Down(i));
                    }
                    if ui
                        .add(egui::Button::new("Remove").small())
                        .on_hover_text("Remove the track")
                        .clicked()
                    {
                        action = Some(TrackAction::Remove(i));
                    }
                });
                ui.end_row();
            }
        });
    ui.add_space(4.0);
    if ui.button("Add a track").clicked() {
        action = Some(TrackAction::Add);
    }
    hint(
        ui,
        "One track per line, in order. Montage's own sounds are always left out of tracks that \
         record the desktop.",
    );

    let Some(action) = action else {
        return;
    };
    pad_track_names(draft);
    let audio = &mut draft.recorder.audio;
    let names = &mut draft.track_names;
    match action {
        TrackAction::Up(i) => {
            audio.swap(i - 1, i);
            names.swap(i - 1, i);
        }
        TrackAction::Down(i) => {
            audio.swap(i, i + 1);
            names.swap(i, i + 1);
        }
        TrackAction::Remove(i) => {
            audio.remove(i);
            names.remove(i);
        }
        TrackAction::Add => {
            let next = ["default_output", "default_input"]
                .into_iter()
                .find(|s| !audio.iter().any(|a| a == s))
                .unwrap_or("default_input");
            audio.push(next.to_string());
            names.truncate(audio.len() - 1);
            names.push(match next {
                "default_output" => "Game".into(),
                "default_input" => "Mic".into(),
                _ => format!("Track {}", audio.len()),
            });
        }
    }
}

/// Gives every track a name entry, so the names can move with the tracks.
fn pad_track_names(draft: &mut Config) {
    let count = draft.recorder.audio.len();
    while draft.track_names.len() < count {
        let n = draft.track_names.len();
        draft.track_names.push(draft.track_name(n));
    }
}

fn source_picker(
    ui: &mut egui::Ui,
    index: usize,
    label: egui::Id,
    source: &mut String,
    lists: Option<&Probe>,
) {
    let mut options: Vec<(String, String)> = vec![
        (
            "default_output".into(),
            source_label("default_output", None),
        ),
        ("default_input".into(), source_label("default_input", None)),
    ];
    if let Some(Ok(devices)) = lists.map(|l| &l.audio_devices) {
        for d in devices {
            if d.name != "default_output" && d.name != "default_input" {
                options.push((d.name.clone(), d.description.clone()));
            }
        }
    }
    if let Some(Ok(apps)) = lists.map(|l| &l.apps) {
        for app in apps {
            options.push((format!("app:{app}"), format!("App: {app}")));
        }
    }
    egui::ComboBox::from_id_salt(("audio-source", index))
        .width(260.0)
        .selected_text(source_label(source, lists))
        .show_ui(ui, |ui| {
            for (value, label) in options {
                ui.selectable_value(source, value, label);
            }
        })
        .response
        .labelled_by(label);
}

/// What a track's source is called: `Desktop audio`, `App: firefox`, a
/// device's description; parts joined with `|` are added up.
fn source_label(source: &str, lists: Option<&Probe>) -> String {
    if source.trim().is_empty() {
        return "(none)".into();
    }
    let devices = lists.and_then(|l| l.audio_devices.as_ref().ok());
    source
        .split('|')
        .map(|part| match part {
            "default_output" => "Desktop audio".to_string(),
            "default_input" => "Default microphone".to_string(),
            _ => {
                if let Some(app) = part.strip_prefix("app:") {
                    format!("App: {app}")
                } else if let Some(app) = part.strip_prefix("app-inverse:") {
                    format!("All but {app}")
                } else {
                    let name = part.strip_prefix("device:").unwrap_or(part);
                    devices
                        .and_then(|ds| ds.iter().find(|d| d.name == name))
                        .map(|d| d.description.clone())
                        .unwrap_or_else(|| name.to_string())
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

/// Whether each part of a source is in the lists (or the lists aren't
/// there to say).
fn source_known(source: &str, lists: Option<&Probe>) -> bool {
    let Some(lists) = lists else {
        return true;
    };
    source.split('|').all(|part| {
        if part == "default_output" || part == "default_input" || part.starts_with("app-inverse:") {
            return true;
        }
        if let Some(app) = part.strip_prefix("app:") {
            return match &lists.apps {
                Ok(apps) => apps.iter().any(|a| a.eq_ignore_ascii_case(app)),
                Err(_) => true,
            };
        }
        let name = part.strip_prefix("device:").unwrap_or(part);
        match &lists.audio_devices {
            Ok(devices) => devices.iter().any(|d| d.name == name),
            Err(_) => true,
        }
    })
}

fn folders(ui: &mut egui::Ui, draft: &mut Config, checks: &mut HashMap<PathBuf, FolderCheck>) {
    let r = &mut draft.recorder;
    grid("folders").show(ui, |ui| {
        let label = ui.label("Replays").id;
        folder_row(ui, "replay-dir", label, &mut r.replay_dir, Some(checks));
        ui.end_row();
        let label = ui.label("Recordings").id;
        folder_row(ui, "record-dir", label, &mut r.record_dir, Some(checks));
        ui.end_row();
    });
    hint(
        ui,
        "gpu-screen-recorder saves into a folder per day inside these, and Montage gives each clip \
         a folder of its own.",
    );
}

/// A path field with a folder picker, and the result of the writability
/// check when `checks` is given (else whether the folder exists).
fn folder_row(
    ui: &mut egui::Ui,
    id: &str,
    label: egui::Id,
    path: &mut PathBuf,
    checks: Option<&mut HashMap<PathBuf, FolderCheck>>,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let mut text = path.display().to_string();
        if ui
            .add(
                egui::TextEdit::singleline(&mut text)
                    .id_salt(id)
                    .desired_width(380.0),
            )
            .labelled_by(label)
            .changed()
        {
            *path = PathBuf::from(text);
            changed = true;
        }
        if ui.button("Choose…").clicked()
            && let Some(picked) = pick_folder(path)
        {
            *path = picked;
            changed = true;
        }
        match checks {
            Some(checks) => {
                let check = checks
                    .entry(path.clone())
                    .or_insert_with(|| apply::check_folder(path));
                match check {
                    FolderCheck::Writable => {
                        ui.colored_label(Color32::from_rgb(70, 170, 90), "writable");
                    }
                    FolderCheck::WillBeCreated => {
                        ui.weak("will be created");
                    }
                    FolderCheck::Problem(p) => {
                        ui.colored_label(ui.visuals().error_fg_color, p.as_str());
                    }
                }
            }
            None => {
                if !path.is_dir() {
                    ui.weak("doesn't exist");
                }
            }
        }
    });
    changed
}

fn pick_folder(current: &Path) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title("Choose a folder");
    if let Some(start) = current.ancestors().find(|p| p.is_dir()) {
        dialog = dialog.set_directory(start);
    }
    dialog.pick_folder()
}

fn sharing(ui: &mut egui::Ui, draft: &mut Config) {
    let s = &mut draft.share;
    grid("sharing").show(ui, |ui| {
        ui.label("Size limit");
        ui.horizontal(|ui| {
            let mut mib = s.max_bytes as f64 / MIB;
            let drag = egui::DragValue::new(&mut mib)
                .range(1.0..=4096.0)
                .speed(0.5)
                .max_decimals(1)
                .suffix(" MiB");
            if ui.add(drag).changed() {
                s.max_bytes = (mib * MIB).round() as u64;
            }
            hint(ui, "the most a shared clip weighs");
        });
        ui.end_row();

        ui.label("Frame rate");
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut s.fps)
                    .range(1..=240)
                    .suffix(" fps"),
            );
            hint(ui, "at most");
        });
        ui.end_row();

        ui.label("Audio");
        ui.add(
            egui::DragValue::new(&mut s.audio_kbps)
                .range(32..=512)
                .suffix(" kbit/s"),
        );
        ui.end_row();

        ui.label("Lowest video bitrate");
        ui.horizontal(|ui| {
            kbps(ui, &mut s.min_video_kbps);
            hint(ui, "a longer clip is cut instead");
        });
        ui.end_row();

        ui.label("1080p from");
        ui.horizontal(|ui| {
            kbps(ui, &mut s.hd_video_kbps);
            hint(ui, "below this, 720p");
        });
        ui.end_row();

        ui.label("Highest video bitrate");
        ui.horizontal(|ui| {
            kbps(ui, &mut s.max_video_kbps);
            hint(ui, "however short the clip");
        });
        ui.end_row();
    });
}

fn kbps(ui: &mut egui::Ui, value: &mut u32) {
    ui.add(
        egui::DragValue::new(value)
            .range(100..=100_000)
            .speed(50)
            .suffix(" kbit/s"),
    );
}

fn gif(ui: &mut egui::Ui, draft: &mut Config) {
    let g = &mut draft.gif;
    grid("gif").show(ui, |ui| {
        ui.label("Width");
        ui.add(
            egui::DragValue::new(&mut g.width)
                .range(64..=1920)
                .suffix(" px"),
        );
        ui.end_row();
        ui.label("Frame rate");
        ui.add(
            egui::DragValue::new(&mut g.fps)
                .range(1..=60)
                .suffix(" fps"),
        );
        ui.end_row();
        ui.label("Size target");
        ui.horizontal(|ui| {
            let mut mb = g.max_bytes as f64 / 1e6;
            let drag = egui::DragValue::new(&mut mb)
                .range(0.5..=500.0)
                .speed(0.2)
                .max_decimals(1)
                .suffix(" MB");
            if ui.add(drag).changed() {
                g.max_bytes = (mb * 1e6).round() as u64;
            }
            hint(ui, "the width, then the frame rate, go down until it fits");
        });
        ui.end_row();
    });
}

fn sounds(ui: &mut egui::Ui, draft: &mut Config) {
    let s = &mut draft.sounds;
    ui.checkbox(
        &mut s.enabled,
        "Play a sound when a recording starts, and when a clip is saved",
    );
    ui.add_enabled_ui(s.enabled, |ui| {
        ui.horizontal(|ui| {
            let label = ui.label("Volume").id;
            ui.add(egui::Slider::new(&mut s.volume, 0.0..=1.0).show_value(false))
                .labelled_by(label);
            ui.label(format!("{:.0} %", s.volume * 100.0));
        });
    });
}

fn library_folders(ui: &mut egui::Ui, draft: &mut Config, recorder: bool) {
    let mut use_default = draft.library.is_none();
    let default_text = if recorder {
        "Use the replays and recordings folders"
    } else if cfg!(target_os = "macos") {
        "Use the Movies folder"
    } else {
        "Use the Videos folder"
    };
    if ui.checkbox(&mut use_default, default_text).changed() {
        draft.library = if use_default {
            None
        } else {
            Some(draft.library_folders())
        };
    }
    match &mut draft.library {
        None => {
            let defaults = draft.library_folders();
            for folder in &defaults {
                ui.label(
                    RichText::new(folder.display().to_string())
                        .monospace()
                        .weak(),
                );
            }
            if defaults.is_empty() {
                hint(
                    ui,
                    "This system has no Videos folder: choose folders instead.",
                );
            }
        }
        Some(list) => {
            let mut remove = None;
            for (i, folder) in list.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    let label = ui.label("Folder").id;
                    folder_row(ui, &format!("library-{i}"), label, folder, None);
                    if ui.small_button("Remove").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                list.remove(i);
            }
            if ui.button("Add a folder…").clicked()
                && let Some(folder) = pick_folder(Path::new(""))
                && !list.contains(&folder)
            {
                list.push(folder);
            }
        }
    }
    hint(
        ui,
        if recorder {
            "The library shows the clips in these folders. Set your own only to look elsewhere."
        } else {
            "The library shows the clips in these folders, and the day folders inside them. Add \
             the folder each recorder saves to."
        },
    );
}

#[cfg(test)]
mod tests {
    use super::probe::{AudioDevice, Monitor, Output};
    use super::*;
    use crate::ui::FontsHandle;
    use egui_kittest::kittest::Queryable;

    fn fake_lists() -> Probe {
        Probe {
            monitors: Ok(vec![
                Monitor {
                    name: "DP-5".into(),
                    size: Some("2560x1440".into()),
                },
                Monitor {
                    name: "HDMI-A-1".into(),
                    size: Some("1920x1080".into()),
                },
            ]),
            audio_devices: Ok(vec![
                AudioDevice {
                    name: "default_output".into(),
                    description: "Default output".into(),
                },
                AudioDevice {
                    name: "alsa_input.usb-Blue_Yeti".into(),
                    description: "Yeti Stereo Microphone".into(),
                },
            ]),
            apps: Ok(vec!["firefox".into(), "Discord".into()]),
            outputs: vec![Output {
                name: "DP-5".into(),
                model: "Dell U2720Q".into(),
            }],
        }
    }

    fn config(tmp: &Path) -> Config {
        let mut config = Config::default();
        config.recorder.monitor = "DP-5".into();
        config.recorder.fps = 144;
        config.recorder.replay_dir = tmp.join("Replays");
        config.recorder.record_dir = tmp.join("Recordings");
        config.recorder.audio = vec![
            "default_output".into(),
            "default_input".into(),
            "app:steam".into(),
        ];
        config.track_names = vec!["Game".into(), "Mic".into()];
        std::fs::create_dir_all(tmp.join("Replays")).unwrap();
        config
    }

    struct State {
        view: SettingsView,
        shared: Shared,
        nav: Option<Nav>,
    }

    fn harness(
        config: Config,
        recorder: bool,
        size: egui::Vec2,
    ) -> egui_kittest::Harness<'static, State> {
        let mut shared = Shared {
            config,
            fonts: FontsHandle::default(),
        };
        let ctx = egui::Context::default();
        let view = SettingsView::with_options(&ctx, &mut shared, recorder, Some(fake_lists()));
        let mut harness = egui_kittest::Harness::builder()
            .with_size(size)
            .wgpu()
            .build_ui_state(
                |ui, state: &mut State| {
                    if let Some(nav) = state.view.ui(ui, &mut state.shared) {
                        state.nav = Some(nav);
                    }
                },
                State {
                    view,
                    shared,
                    nav: None,
                },
            );
        harness.run_steps(3);
        harness
    }

    #[test]
    fn linux_settings_render() {
        let tmp = tempfile::tempdir().unwrap();
        let mut harness = harness(config(tmp.path()), true, egui::vec2(1000.0, 1900.0));
        harness
            .render()
            .unwrap()
            .save(crate::ui::library::tests::snapshot_dir().join("settings-linux.png"))
            .unwrap();
        assert!(!harness.state().view.dirty());
        assert!(harness.state_mut().view.problems().is_empty());
    }

    #[test]
    fn other_systems_show_what_applies_to_them() {
        let tmp = tempfile::tempdir().unwrap();
        let mut harness = harness(config(tmp.path()), false, egui::vec2(1000.0, 1000.0));
        harness
            .render()
            .unwrap()
            .save(crate::ui::library::tests::snapshot_dir().join("settings-other.png"))
            .unwrap();
        assert!(harness.query_by_label("Capture").is_none());
        assert!(harness.query_by_label("Sharing").is_some());
        assert!(harness.query_by_label("Library folders").is_some());
    }

    #[test]
    fn leaving_with_changes_asks_first_and_recorder_changes_ask_to_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let mut harness = harness(config(tmp.path()), true, egui::vec2(1000.0, 800.0));
        harness.state_mut().view.draft.recorder.replay_seconds = 120;
        harness.run_steps(2);
        assert!(harness.state().view.needs_restart());
        harness.key_press(egui::Key::Escape);
        harness.run_steps(2);
        assert!(harness.state().nav.is_none());
        assert!(matches!(harness.state().view.dialog, Some(Dialog::Discard)));
        harness.get_by_label("Keep editing").click();
        harness.run_steps(2);
        assert!(harness.state().view.dialog.is_none());

        harness.state_mut().view.dialog = Some(Dialog::Restart { recording: true });
        harness.run_steps(2);
        harness
            .render()
            .unwrap()
            .save(crate::ui::library::tests::snapshot_dir().join("settings-restart.png"))
            .unwrap();
        assert!(
            harness
                .query_by_label("Apply after the recording stops")
                .is_some()
        );
        // Escape closes the dialog, and stays on the page.
        harness.key_press(egui::Key::Escape);
        harness.run_steps(2);
        assert!(harness.state().view.dialog.is_none());
        assert!(harness.state().nav.is_none());

        // Discarding goes back without saving.
        harness.state_mut().view.leave();
        harness.run_steps(3);
        harness.get_by_label("Discard").click();
        harness.run_steps(2);
        assert!(matches!(harness.state().nav, Some(Nav::Library)));
        assert_eq!(harness.state().shared.config.recorder.replay_seconds, 60);
    }

    #[test]
    fn problems_block_saving() {
        let tmp = tempfile::tempdir().unwrap();
        let mut shared = Shared {
            config: config(tmp.path()),
            fonts: FontsHandle::default(),
        };
        let ctx = egui::Context::default();
        let mut view = SettingsView::with_options(&ctx, &mut shared, true, Some(fake_lists()));
        assert!(view.problems().is_empty());
        view.draft.recorder.monitor = "DP-9".into();
        assert_eq!(view.problems(), ["No screen named DP-9 is connected."]);
        view.draft.recorder.monitor = "screen".into();
        view.draft.recorder.audio[1] = " ".into();
        view.draft.share.min_video_kbps = 9000;
        let file = tmp.path().join("file");
        std::fs::write(&file, b"").unwrap();
        view.draft.recorder.record_dir = file;
        let problems = view.problems();
        assert_eq!(problems.len(), 3, "{problems:?}");
        // Off Linux the recorder isn't checked.
        view.recorder = false;
        assert_eq!(view.problems().len(), 1);
    }

    #[test]
    fn tracks_move_with_their_names() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = config(tmp.path());
        pad_track_names(&mut c);
        assert_eq!(c.track_names, ["Game", "Mic", "Track 3"]);
        assert_eq!(
            source_label("default_output|app:firefox", Some(&fake_lists())),
            "Desktop audio + App: firefox"
        );
        assert_eq!(
            source_label("alsa_input.usb-Blue_Yeti", Some(&fake_lists())),
            "Yeti Stereo Microphone"
        );
        assert!(source_known(
            "app:Firefox|default_input",
            Some(&fake_lists())
        ));
        assert!(!source_known("app:steam", Some(&fake_lists())));
        assert!(source_known("app:steam", None));
        assert_eq!(
            screen_label("DP-5", Some(&fake_lists())),
            "DP-5 — Dell U2720Q · 2560x1440"
        );
        assert_eq!(
            screen_label("DP-9", Some(&fake_lists())),
            "DP-9 (not connected)"
        );
    }
}
