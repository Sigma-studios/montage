//! The view drawn with a fake player and made-up thumbnails, saved as PNGs
//! under `target/agent-editor/snapshots/` to look at, plus the keyboard
//! driving it.

use std::path::PathBuf;

use eframe::egui;
use egui_kittest::Harness;
use montage_core::edit::{Caption, Cut, TrackSettings};
use montage_core::render::RgbaImage;
use montage_core::{Clip, Config, Edit};
use montage_media::{ExportPreset, MediaInfo, Waveform};

use super::playback::{FakePlayer, PlayerSlot};
use super::thumbs::Thumbs;
use super::*;
use crate::ui::{FontsHandle, Shared};

fn snapshot_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/agent-editor/snapshots")
}

fn info() -> MediaInfo {
    MediaInfo {
        duration: 12.0,
        width: 2560,
        height: 1440,
        fps: 144.0,
        video_codec: "hevc".into(),
        audio_tracks: 2,
        bit_rate: 60_000_000,
    }
}

/// A frame that looks a bit like a game: sky, ground, a sun.
fn frame(width: u32, height: u32, shift: u32) -> RgbaImage {
    let mut image = RgbaImage::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            let fy = y as f32 / height as f32;
            let fx = (x + shift * 7) as f32 / width as f32;
            let (r, g, b) = if fy < 0.62 {
                (40.0 + 60.0 * fy, 90.0 + 90.0 * fy, 160.0 + 70.0 * fy)
            } else {
                let stripe = ((fx * 24.0).sin() * 0.5 + 0.5) * 30.0;
                (60.0 + stripe, 110.0 + stripe, 50.0)
            };
            let (dx, dy) = (fx.fract() - 0.7, fy - 0.25);
            let sun = (dx * dx + dy * dy).sqrt() < 0.06;
            let px = if sun {
                [250, 220, 120]
            } else {
                [r as u8, g as u8, b as u8]
            };
            image.data[i..i + 4].copy_from_slice(&[px[0], px[1], px[2], 255]);
        }
    }
    image
}

fn waveform() -> Waveform {
    let buckets = 600;
    let track = |speed: f32, amp: f32| {
        (0..buckets)
            .map(|i| {
                let t = i as f32 / buckets as f32;
                let a = amp * (0.35 + 0.65 * ((t * speed).sin() * (t * 37.0).cos()).abs());
                (-a, a * 0.9)
            })
            .collect()
    };
    Waveform {
        bucket_seconds: 12.0 / buckets as f64,
        tracks: vec![track(9.0, 0.5), track(23.0, 0.3)],
    }
}

fn fake_estimate(_: &MediaInfo, preset: &ExportPreset, cut: Cut) -> u64 {
    let per_second = match preset {
        ExportPreset::Share { .. } => 1_100_000.0,
        ExportPreset::Full => 6_000_000.0,
        ExportPreset::Gif(_) => 700_000.0,
    };
    (per_second * cut.duration()) as u64
}

struct State {
    view: EditorView,
    shared: Shared,
    went_back: bool,
    _tmp: tempfile::TempDir,
}

fn editor(edit: Edit, player: PlayerSlot, thumbs: bool) -> State {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("Replay_2026-11-07_21-03-11");
    std::fs::create_dir_all(&folder).unwrap();
    let clip = Clip {
        original: folder.join("Replay_2026-11-07_21-03-11.mp4"),
        in_folder: true,
    };
    let shared = Shared {
        config: Config::default(),
        fonts: FontsHandle::default(),
    };
    let frames = (0..thumbs::FILMSTRIP_COUNT as u32)
        .map(|i| frame(192, 108, i * 9))
        .collect();
    let thumbs = if thumbs {
        Thumbs::ready(frames, waveform())
    } else {
        Thumbs::loading()
    };
    let editor = Editor::from_parts(&shared, clip, edit, info(), player, thumbs, fake_estimate);
    State {
        view: EditorView {
            path: folder,
            state: Ok(Box::new(editor)),
        },
        shared,
        went_back: false,
        _tmp: tmp,
    }
}

fn harness(state: State) -> Harness<'static, State> {
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1280.0, 860.0))
        .build_ui_state(
            |ui, state: &mut State| {
                if state.view.ui(ui, &mut state.shared).is_some() {
                    state.went_back = true;
                }
            },
            state,
        );
    harness.run_steps(3);
    harness
}

fn fake_player() -> PlayerSlot {
    PlayerSlot::Open(Box::new(FakePlayer::new(frame(1280, 720, 0))))
}

fn edited() -> Edit {
    let mut edit = Edit {
        cut: Some(Cut {
            start: 3.0,
            end: 9.5,
        }),
        ..Edit::default()
    };
    edit.set_track(TrackSettings {
        index: 1,
        muted: true,
        volume: 1.0,
    });
    edit.set_track(TrackSettings {
        index: 0,
        muted: false,
        volume: 0.8,
    });
    edit.captions.push(Caption {
        text: "nice shot".into(),
        start: Some(4.0),
        end: Some(7.5),
        ..Caption::default()
    });
    edit.captions.push(Caption {
        text: "gg".into(),
        y: 0.88,
        ..Caption::default()
    });
    edit
}

fn save_png(harness: &mut Harness<State>, name: &str) -> PathBuf {
    let dir = snapshot_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.png"));
    let image = harness.render().expect("render the editor");
    image.save(&path).unwrap();
    path
}

fn editor_mut<'h>(harness: &'h mut Harness<State>) -> &'h mut Editor {
    harness.state_mut().view.state.as_mut().ok().unwrap()
}

fn harness_with(edit: Edit, thumbs: bool) -> Harness<'static, State> {
    harness(editor(edit, fake_player(), thumbs))
}

#[test]
fn snapshot_of_the_editor() {
    let mut harness = harness_with(edited(), true);
    editor_mut(&mut harness).seek(5.0);
    editor_mut(&mut harness).selected = Some(0);
    harness.run_steps(3);
    let path = save_png(&mut harness, "editor");
    assert!(path.exists());
}

#[test]
fn snapshot_while_loading() {
    let mut harness = harness(editor(Edit::default(), PlayerSlot::Pending, false));
    save_png(&mut harness, "editor-loading");
    let mut harness = harness_with(Edit::default(), false);
    editor_mut(&mut harness).export.choice = super::export::PresetChoice::Gif;
    harness.run_steps(2);
    save_png(&mut harness, "editor-gif");
}

#[test]
fn keys_drive_the_player_and_the_cut() {
    let mut harness = harness_with(Edit::default(), true);
    let playing = |h: &mut Harness<State>| editor_mut(h).player.get_ref().unwrap().is_playing();

    harness.key_press(egui::Key::Space);
    harness.run_steps(1);
    assert!(playing(&mut harness));
    harness.key_press(egui::Key::K);
    harness.run_steps(1);
    assert!(!playing(&mut harness));

    editor_mut(&mut harness).seek(2.0);
    harness.key_press(egui::Key::I);
    harness.run_steps(1);
    editor_mut(&mut harness).seek(8.0);
    harness.key_press(egui::Key::O);
    harness.run_steps(1);
    assert_eq!(
        editor_mut(&mut harness).edit.cut,
        Some(Cut {
            start: 2.0,
            end: 8.0
        })
    );

    // Escape saves and goes back to the library.
    harness.key_press(egui::Key::Escape);
    harness.run_steps(1);
    assert!(harness.state().went_back);
    let editor = editor_mut(&mut harness);
    let saved = Edit::load(&editor.clip.edit_path()).unwrap().unwrap();
    assert_eq!(saved.cut, editor.edit.cut);
}

#[test]
fn loop_and_gains_reach_the_player() {
    let mut harness = harness_with(edited(), true);
    let editor = editor_mut(&mut harness);
    editor.looping = true;
    harness.run_steps(1);
    let editor = editor_mut(&mut harness);
    assert_eq!(editor.sent_loop, Some(Some((3.0, 9.5))));
    assert_eq!(editor.sent_gains, Some(vec![0.8, 0.0]));
}

#[test]
fn whole_clip_cuts_are_not_stored() {
    let mut edit = Edit::default();
    store_cut(
        &mut edit,
        Cut {
            start: 0.0,
            end: 12.0,
        },
        12.0,
    );
    assert_eq!(edit.cut, None);
    let cut = Cut {
        start: 1.0,
        end: 12.0,
    };
    store_cut(&mut edit, cut, 12.0);
    assert_eq!(edit.cut, Some(cut));
}

#[test]
fn status_line_describes_the_clip() {
    let cut = Cut {
        start: 1.0,
        end: 4.5,
    };
    assert_eq!(
        clip_summary(&info(), cut),
        "2560×1440 · 144 fps · 0:12.00 · cut 0:03.50"
    );
    assert_eq!(trim_fps(29.97), "29.97");
}

/// The installed fonts, loaded once for every test that needs them.
fn real_fonts() -> FontsHandle {
    use std::sync::{Arc, Mutex, OnceLock};
    static FONTS: OnceLock<Arc<Mutex<montage_text::Fonts>>> = OnceLock::new();
    let fonts = FONTS
        .get_or_init(|| Arc::new(Mutex::new(montage_text::Fonts::load())))
        .clone();
    let handle = FontsHandle::default();
    *handle.loaded.lock().unwrap() = Some(fonts);
    handle
}

#[test]
fn snapshot_with_captions_and_the_font_picker() {
    use egui_kittest::kittest::Queryable;

    let mut edit = edited();
    edit.captions.push(Caption {
        text: "where's my font".into(),
        font: "Nonexistent Font".into(),
        uppercase: false,
        size: 0.07,
        y: 0.5,
        shadow: true,
        ..Caption::default()
    });
    let mut state = editor(edit, fake_player(), true);
    state.shared.fonts = real_fonts();
    let mut harness = harness(state);
    editor_mut(&mut harness).seek(5.0);
    editor_mut(&mut harness).selected = Some(2);
    harness.run_steps(3);
    save_png(&mut harness, "editor-captions");

    // The missing font is reported.
    harness.get_by_label_contains("isn't installed on this computer");

    harness.get_by_label("Nonexistent Font").click();
    harness.run_steps(12);
    save_png(&mut harness, "editor-font-picker");
    harness.get_by_label("Search fonts");
}

/// A 4 s clip with two audio tracks, made with the ffmpeg program; `None`
/// when it isn't installed.
pub(super) fn make_test_clip(dir: &std::path::Path, name: &str) -> Option<PathBuf> {
    let out = dir.join(format!("{name}.mp4"));
    let status = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30:duration=4",
        ])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=4"])
        .args(["-f", "lavfi", "-i", "sine=frequency=660:duration=4"])
        .args(["-map", "0:v", "-map", "1:a", "-map", "2:a"])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
        .arg(&out)
        .status()
        .ok()?;
    status.success().then_some(out)
}

pub(super) fn shared_real_fonts() -> std::sync::Arc<std::sync::Mutex<montage_text::Fonts>> {
    real_fonts().get().unwrap()
}

/// Steps the harness in real time until `done`, for at most `secs`.
fn run_until(
    harness: &mut Harness<State>,
    secs: f64,
    done: impl Fn(&mut Harness<State>) -> bool,
) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed().as_secs_f64() < secs {
        harness.step();
        if done(harness) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    false
}

#[test]
fn a_real_clip_opens_plays_and_saves() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(path) = make_test_clip(tmp.path(), "Replay_2026-11-07_10-00-00") else {
        eprintln!("skipped: no ffmpeg program to make the clip");
        return;
    };
    let mut shared = Shared {
        config: Config::default(),
        fonts: real_fonts(),
    };
    let ctx = egui::Context::default();
    let view = EditorView::new(&ctx, &mut shared, path.clone());
    let editor = view.state.as_ref().expect("the clip opens");
    // Adopted into its own folder.
    assert!(editor.clip.in_folder);
    assert!(!path.exists());
    assert!(editor.clip.original.exists());
    assert_eq!(editor.info.audio_tracks, 2);
    assert!((editor.info.duration - 4.0).abs() < 0.2);

    let mut harness = harness(State {
        view,
        shared,
        went_back: false,
        _tmp: tmp,
    });
    let ready = run_until(&mut harness, 30.0, |h| {
        let e = editor_mut(h);
        matches!(e.thumbs.filmstrip, Loadable::Ready(_))
            && matches!(e.thumbs.waveform, Loadable::Ready(_))
    });
    assert!(ready, "thumbnails and waveform made in time");
    if let PlayerSlot::Failed(e) = &editor_mut(&mut harness).player {
        panic!("the player didn't open: {e}");
    }
    assert!(editor_mut(&mut harness).clip.thumbs_dir().exists());

    harness.key_press(egui::Key::Space);
    let played = run_until(&mut harness, 5.0, |h| editor_mut(h).position > 0.4);
    assert!(played, "playback moves the playhead");
    harness.key_press(egui::Key::K);
    harness.run_steps(1);
    let e = editor_mut(&mut harness);
    assert!(!e.player.get_ref().unwrap().is_playing());

    e.seek(1.0);
    harness.key_press(egui::Key::I);
    harness.step();
    editor_mut(&mut harness).seek(3.0);
    harness.key_press(egui::Key::O);
    harness.step();
    run_until(&mut harness, 2.0, |_| false);
    save_png(&mut harness, "editor-real");

    harness.key_press(egui::Key::Escape);
    harness.step();
    assert!(harness.state().went_back);
    let e = editor_mut(&mut harness);
    let saved = Edit::load(&e.clip.edit_path()).unwrap().unwrap();
    let cut = saved.cut.expect("the cut is saved");
    assert!(
        (cut.start - 1.0).abs() < 0.05 && (cut.end - 3.0).abs() < 0.05,
        "{cut:?}"
    );
}
