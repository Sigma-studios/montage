//! The export bar: the preset (Share, Full quality, GIF and its settings),
//! the destination (the clipboard or a file), the estimated size, and the
//! export itself, on a thread, with a progress bar and Cancel.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Align, Layout, RichText};
use montage_core::edit::{Caption, Cut};
use montage_core::presets::GifSettings;
use montage_core::render::Overlay;
use montage_core::share::{self, ClipKind, SharePlan};
use montage_core::{Clip, Config, ExportKind};
use montage_media::{ExportJob, ExportPreset, MediaError, MediaInfo};
use montage_text::{CaptionOverlay, Fonts};

use super::style;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetChoice {
    Share,
    Full,
    Gif,
}

impl PresetChoice {
    pub const ALL: [PresetChoice; 3] = [PresetChoice::Share, PresetChoice::Full, PresetChoice::Gif];

    pub fn label(self) -> &'static str {
        match self {
            PresetChoice::Share => "Share (MP4)",
            PresetChoice::Full => "Full quality (MP4)",
            PresetChoice::Gif => "GIF",
        }
    }

    /// Which name the export gets in the clip's folder.
    pub fn kind(self) -> ExportKind {
        match self {
            PresetChoice::Share => ExportKind::Share,
            PresetChoice::Full => ExportKind::Edit,
            PresetChoice::Gif => ExportKind::Gif,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            PresetChoice::Gif => "gif",
            _ => "mp4",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    Clipboard,
    File,
}

/// `path` with `extension` added when the save dialog returned a name
/// without one.
pub fn with_extension(path: PathBuf, extension: &str) -> PathBuf {
    let has = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(extension));
    if has {
        path
    } else {
        let mut name = path.into_os_string();
        name.push(".");
        name.push(extension);
        PathBuf::from(name)
    }
}

pub fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

/// The notification's text: the file's name and size.
pub fn description(output: &Path, bytes: u64) -> String {
    let name = output.file_name().unwrap_or_default().to_string_lossy();
    format!("{name} · {}", megabytes(bytes))
}

/// The Share plan for this clip and cut.
pub fn share_plan(config: &Config, info: &MediaInfo, clip_name: &str, cut: Cut) -> SharePlan {
    share::plan(
        &config.share,
        info.source(),
        ClipKind::guess(clip_name),
        Some(cut),
    )
}

pub fn build_preset(
    choice: PresetChoice,
    gif: GifSettings,
    config: &Config,
    info: &MediaInfo,
    clip_name: &str,
    cut: Cut,
) -> ExportPreset {
    match choice {
        PresetChoice::Share => ExportPreset::Share {
            plan: share_plan(config, info, clip_name, cut),
            max_bytes: config.share.max_bytes,
        },
        PresetChoice::Full => ExportPreset::Full,
        PresetChoice::Gif => ExportPreset::Gif(gif),
    }
}

pub type EstimateFn = fn(&MediaInfo, &ExportPreset, Cut) -> u64;

/// What the export needs from the editor.
pub struct ExportContext<'a> {
    pub clip: &'a Clip,
    pub info: &'a MediaInfo,
    pub config: &'a Config,
    pub cut: Cut,
    pub gains: Vec<f32>,
    pub captions: &'a [Caption],
    pub fonts: Option<Arc<Mutex<Fonts>>>,
}

impl ExportContext<'_> {
    fn clip_name(&self) -> String {
        self.clip.name().to_string_lossy().into_owned()
    }

    /// The captions with something to draw.
    fn shown_captions(&self) -> Vec<Caption> {
        self.captions
            .iter()
            .filter(|c| !c.text.trim().is_empty())
            .cloned()
            .collect()
    }

    /// Why Export can't run now, if it can't.
    fn blocked(&self) -> Option<&'static str> {
        if self.fonts.is_none() && !self.shown_captions().is_empty() {
            Some("Waiting for the fonts to load")
        } else {
            None
        }
    }
}

struct Finished {
    output: PathBuf,
    bytes: u64,
    method: String,
    copied: bool,
}

enum Outcome {
    Done(Finished),
    Cancelled,
    Failed(String),
}

struct Running {
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
    rx: Receiver<Outcome>,
    output: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct EstimateKey {
    choice: PresetChoice,
    gif: GifSettings,
    cut: Cut,
}

pub struct ExportBar {
    pub choice: PresetChoice,
    pub gif: GifSettings,
    pub destination: Destination,
    estimate: EstimateFn,
    estimated: Option<(EstimateKey, u64, Option<String>)>,
    running: Option<Running>,
    last: Option<Outcome>,
}

impl ExportBar {
    pub fn new(config: &Config, estimate: EstimateFn) -> ExportBar {
        ExportBar {
            choice: PresetChoice::Share,
            gif: config.gif,
            destination: Destination::Clipboard,
            estimate,
            estimated: None,
            running: None,
            last: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Takes in the export thread's result.
    pub fn poll(&mut self) {
        let Some(running) = &self.running else { return };
        match running.rx.try_recv() {
            Ok(outcome) => {
                self.last = Some(outcome);
                self.running = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.last = Some(Outcome::Failed(
                    "the export stopped with an internal error".into(),
                ));
                self.running = None;
            }
        }
    }

    /// The size estimate for the current settings, and a note (how much
    /// Share trims).
    fn estimate(&mut self, cx: &ExportContext) -> (u64, Option<String>) {
        let key = EstimateKey {
            choice: self.choice,
            gif: self.gif,
            cut: cx.cut,
        };
        if let Some((k, bytes, note)) = &self.estimated
            && *k == key
        {
            return (*bytes, note.clone());
        }
        let preset = build_preset(
            self.choice,
            self.gif,
            cx.config,
            cx.info,
            &cx.clip_name(),
            cx.cut,
        );
        let bytes = (self.estimate)(cx.info, &preset, cx.cut);
        let note = match &preset {
            ExportPreset::Share { plan, .. } if plan.trimmed(cx.cut) > 0.5 => {
                let kept = match ClipKind::guess(&cx.clip_name()) {
                    ClipKind::Replay => "last",
                    ClipKind::Regular => "first",
                };
                Some(format!(
                    "too long for {}: the {kept} {:.0} s are kept",
                    megabytes(cx.config.share.max_bytes),
                    plan.range.duration()
                ))
            }
            _ => None,
        };
        self.estimated = Some((key, bytes, note.clone()));
        (bytes, note)
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cx: &ExportContext) {
        self.poll();
        ui.horizontal(|ui| {
            style::heading(ui, "Export");
            let running = self.running.is_some();
            ui.add_enabled_ui(!running, |ui| {
                egui::ComboBox::from_id_salt("montage-export-preset")
                    .selected_text(self.choice.label())
                    .width(170.0)
                    .show_ui(ui, |ui| {
                        for choice in PresetChoice::ALL {
                            ui.selectable_value(&mut self.choice, choice, choice.label());
                        }
                    })
                    .response
                    .accessible_name("Export preset");
                if self.choice == PresetChoice::Gif {
                    ui.add(
                        egui::DragValue::new(&mut self.gif.width)
                            .range(120..=1920)
                            .suffix(" px"),
                    )
                    .on_hover_text("Width");
                    ui.add(
                        egui::DragValue::new(&mut self.gif.fps)
                            .range(5..=50)
                            .suffix(" fps"),
                    )
                    .on_hover_text("Frame rate");
                    let mut mb = self.gif.max_bytes as f64 / 1_000_000.0;
                    if ui
                        .add(
                            egui::DragValue::new(&mut mb)
                                .range(1.0..=100.0)
                                .speed(0.1)
                                .fixed_decimals(1)
                                .prefix("≤ ")
                                .suffix(" MB"),
                        )
                        .on_hover_text(
                            "Size target: the width, then the frame rate, go down until it fits",
                        )
                        .changed()
                    {
                        self.gif.max_bytes = (mb * 1_000_000.0).round() as u64;
                    }
                }
                ui.separator();
                for (destination, label) in [
                    (Destination::Clipboard, "Clipboard"),
                    (Destination::File, "File…"),
                ] {
                    if style::choice(ui, self.destination == destination, label).clicked() {
                        self.destination = destination;
                    }
                }
            });
            ui.separator();
            let (bytes, note) = self.estimate(cx);
            ui.label(RichText::new(format!("≈ {}", megabytes(bytes))).color(style::TEXT))
                .on_hover_text("Estimated size");
            if let Some(note) = note {
                ui.label(RichText::new(note).color(style::SUBTEXT));
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(running) = &self.running {
                    if ui.button("Cancel").clicked() {
                        running.cancel.store(true, Ordering::Relaxed);
                    }
                    let progress = f32::from_bits(running.progress.load(Ordering::Relaxed));
                    let name = running
                        .output
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    ui.add(
                        egui::ProgressBar::new(progress)
                            .desired_width(220.0)
                            .show_percentage(),
                    )
                    .on_hover_text(format!("Writing {name}"));
                    if running.cancel.load(Ordering::Relaxed) {
                        ui.label(RichText::new("Cancelling…").color(style::SUBTEXT));
                    }
                } else {
                    let blocked = cx.blocked();
                    let button = egui::Button::new(
                        RichText::new("Export").strong().color(egui::Color32::BLACK),
                    )
                    .fill(style::ACCENT)
                    .min_size([96.0, 30.0].into());
                    let response = ui.add_enabled(blocked.is_none(), button);
                    let response = match blocked {
                        Some(why) => response.on_disabled_hover_text(why),
                        None => response.on_hover_text(if cfg!(target_os = "macos") {
                            "Cmd+E"
                        } else {
                            "Ctrl+E"
                        }),
                    };
                    if response.clicked() {
                        self.start(ui.ctx(), cx);
                    }
                }
            });
        });
    }

    /// The last export's result, for the status line.
    pub fn status(&self) -> Option<(String, egui::Color32)> {
        match &self.last {
            Some(Outcome::Done(done)) => {
                let what = if done.copied { "Copied" } else { "Saved" };
                let text = format!(
                    "{what} {} ({})",
                    description(&done.output, done.bytes),
                    done.method
                );
                Some((text, style::SUBTEXT))
            }
            Some(Outcome::Cancelled) => Some(("Export cancelled".into(), style::SUBTEXT)),
            Some(Outcome::Failed(e)) => Some((format!("Export failed: {e}"), style::ERROR)),
            None => None,
        }
    }

    /// Starts the export with the current settings, asking where to save
    /// it first for a file.
    pub fn start(&mut self, ctx: &egui::Context, cx: &ExportContext) {
        if self.running.is_some() || cx.blocked().is_some() {
            return;
        }
        let kind = self.choice.kind();
        let default = cx.clip.next_export_path(kind);
        let to_clipboard = self.destination == Destination::Clipboard;
        let output = if to_clipboard {
            default
        } else {
            let extension = self.choice.extension();
            let name = default.file_name().unwrap_or_default().to_string_lossy();
            let picked = rfd::FileDialog::new()
                .set_title("Export the clip")
                .set_directory(cx.clip.folder())
                .set_file_name(name)
                .add_filter(extension.to_uppercase(), &[extension])
                .save_file();
            match picked {
                Some(path) => with_extension(path, extension),
                None => return,
            }
        };

        let preset = build_preset(
            self.choice,
            self.gif,
            cx.config,
            cx.info,
            &cx.clip_name(),
            cx.cut,
        );
        let captions = cx.shown_captions();
        let job = Job {
            input: cx.clip.original.clone(),
            output: output.clone(),
            preset,
            cut: cx.cut,
            gains: cx.gains.clone(),
            overlay: match (&cx.fonts, captions.is_empty()) {
                (Some(fonts), false) => Some((fonts.clone(), captions)),
                _ => None,
            },
            to_clipboard,
        };
        let progress = Arc::new(AtomicU32::new(0f32.to_bits()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        {
            let (progress, cancel, ctx) = (progress.clone(), cancel.clone(), ctx.clone());
            std::thread::spawn(move || {
                let outcome = job.run(&progress, &cancel, &ctx);
                let _ = tx.send(outcome);
                ctx.request_repaint();
            });
        }
        self.last = None;
        self.running = Some(Running {
            progress,
            cancel,
            rx,
            output,
        });
    }
}

/// An export, owned by its thread.
struct Job {
    input: PathBuf,
    output: PathBuf,
    preset: ExportPreset,
    cut: Cut,
    gains: Vec<f32>,
    overlay: Option<(Arc<Mutex<Fonts>>, Vec<Caption>)>,
    to_clipboard: bool,
}

impl Job {
    /// Exports, then puts the file on the clipboard (which notifies) or
    /// notifies itself. Done here rather than in the window so it happens
    /// even if the editor was left meanwhile.
    fn run(self, progress: &AtomicU32, cancel: &AtomicBool, ctx: &egui::Context) -> Outcome {
        let mut overlay = self
            .overlay
            .map(|(fonts, captions)| CaptionOverlay::new(fonts, captions, self.cut));
        let job = ExportJob {
            input: &self.input,
            output: &self.output,
            preset: self.preset,
            cut: self.cut,
            gains: &self.gains,
            overlay: overlay.as_mut().map(|o| o as &mut dyn Overlay),
        };
        let mut last_shown = 0.0f32;
        let mut on_progress = |p: f32| {
            progress.store(p.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
            if p - last_shown >= 0.01 || p >= 1.0 {
                last_shown = p;
                ctx.request_repaint();
            }
        };
        let report = match montage_media::export(job, &mut on_progress, cancel) {
            Ok(report) => report,
            Err(MediaError::Cancelled) => return Outcome::Cancelled,
            Err(e) => return Outcome::Failed(e.to_string()),
        };
        let description = description(&self.output, report.bytes);
        if self.to_clipboard {
            if let Err(e) = crate::share::copy_detached(&self.output, &description) {
                return Outcome::Failed(format!(
                    "saved {} but couldn't copy it: {e:#}",
                    self.output.display()
                ));
            }
        } else {
            crate::platform::notify("Clip exported", &description);
        }
        Outcome::Done(Finished {
            output: self.output,
            bytes: report.bytes,
            method: report.method,
            copied: self.to_clipboard,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_preset_has_its_name_and_extension() {
        assert_eq!(PresetChoice::Share.kind(), ExportKind::Share);
        assert_eq!(PresetChoice::Full.kind(), ExportKind::Edit);
        assert_eq!(PresetChoice::Gif.kind(), ExportKind::Gif);
        assert_eq!(PresetChoice::Share.extension(), "mp4");
        assert_eq!(PresetChoice::Gif.extension(), "gif");

        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("Replay_x");
        std::fs::create_dir_all(&folder).unwrap();
        let clip = Clip {
            original: folder.join("Replay_x.mp4"),
            in_folder: true,
        };
        let name = |c: PresetChoice| {
            clip.next_export_path(c.kind())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(name(PresetChoice::Share), "Replay_x-share.mp4");
        assert_eq!(name(PresetChoice::Full), "Replay_x-edit.mp4");
        assert_eq!(name(PresetChoice::Gif), "Replay_x.gif");
    }

    #[test]
    fn save_dialog_names_get_their_extension() {
        assert_eq!(
            with_extension(PathBuf::from("/a/clip"), "gif"),
            PathBuf::from("/a/clip.gif")
        );
        assert_eq!(
            with_extension(PathBuf::from("/a/clip.GIF"), "gif"),
            PathBuf::from("/a/clip.GIF")
        );
        assert_eq!(
            with_extension(PathBuf::from("/a/clip.v2"), "mp4"),
            PathBuf::from("/a/clip.v2.mp4")
        );
    }

    #[test]
    fn share_uses_the_cut_and_the_config() {
        let config = Config::default();
        let info = MediaInfo {
            duration: 120.0,
            width: 2560,
            height: 1440,
            fps: 144.0,
            video_codec: "hevc".into(),
            audio_tracks: 2,
            bit_rate: 60_000_000,
        };
        let cut = Cut {
            start: 10.0,
            end: 20.0,
        };
        let preset = build_preset(
            PresetChoice::Share,
            config.gif,
            &config,
            &info,
            "Replay_x",
            cut,
        );
        let ExportPreset::Share { plan, max_bytes } = preset else {
            panic!("not a Share preset");
        };
        assert_eq!(plan.range, cut);
        assert_eq!(max_bytes, config.share.max_bytes);
        assert_eq!(plan.fps, 60.0);
        // A long replay keeps its end.
        let long = Cut {
            start: 0.0,
            end: 120.0,
        };
        let plan = share_plan(&config, &info, "Replay_x", long);
        assert_eq!(plan.range.end, 120.0);
        assert!(plan.range.start > 0.0);
        let plan = share_plan(&config, &info, "Recording", long);
        assert_eq!(plan.range.start, 0.0);
    }

    #[test]
    fn descriptions_name_the_file_and_size() {
        assert_eq!(
            description(Path::new("/x/Replay_x-share.mp4"), 12_345_678),
            "Replay_x-share.mp4 · 12.3 MB"
        );
    }
}

#[cfg(test)]
mod real_tests {
    use super::*;
    use crate::ui::editor::tests::{make_test_clip, shared_real_fonts};

    fn run(job: Job) -> Finished {
        let progress = AtomicU32::new(0);
        let cancel = AtomicBool::new(false);
        match job.run(&progress, &cancel, &egui::Context::default()) {
            Outcome::Done(done) => {
                assert_eq!(f32::from_bits(progress.load(Ordering::Relaxed)), 1.0);
                done
            }
            Outcome::Cancelled => panic!("cancelled"),
            Outcome::Failed(e) => panic!("export failed: {e}"),
        }
    }

    #[test]
    fn real_exports_apply_the_cut_and_captions() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(input) = make_test_clip(tmp.path(), "Replay_x") else {
            eprintln!("skipped: no ffmpeg program to make the clip");
            return;
        };
        let info = montage_media::probe(&input).unwrap();
        let cut = Cut {
            start: 0.5,
            end: 2.5,
        };
        let config = Config::default();
        let captions = vec![Caption {
            text: "gg".into(),
            ..Caption::default()
        }];
        for choice in PresetChoice::ALL {
            let output = tmp.path().join(format!("out.{}", choice.extension()));
            let done = run(Job {
                input: input.clone(),
                output: output.clone(),
                preset: build_preset(choice, config.gif, &config, &info, "Replay_x", cut),
                cut,
                gains: vec![1.0, 0.0],
                overlay: Some((shared_real_fonts(), captions.clone())),
                to_clipboard: false,
            });
            assert_eq!(done.bytes, std::fs::metadata(&output).unwrap().len());
            let out = montage_media::probe(&output).unwrap();
            assert!(
                (out.duration - 2.0).abs() < 0.2,
                "{choice:?}: {} s ({})",
                out.duration,
                done.method
            );
            let estimate = montage_media::estimate_bytes(
                &info,
                &build_preset(choice, config.gif, &config, &info, "Replay_x", cut),
                cut,
            );
            assert!(estimate > 0, "{choice:?}");
        }
    }
}
