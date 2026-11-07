//! The Full quality and GIF presets, captions, cancelling and size
//! estimates, through `export`, on clips generated at test time.

mod support;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use montage_core::edit::Cut;
use montage_core::presets::GifSettings;
use montage_core::render::{Overlay, RgbaImage};
use montage_core::share::{self, ClipKind, ShareSettings};
use montage_media::{
    ExportJob, ExportPreset, ExportReport, MediaError, estimate_bytes, export, probe,
};
use support::{Spec, audio_stream_count, frame_indices, make_clip, rgba_frames, video_packets};

/// 4 s at 30 fps with a keyframe every 15 frames (0.5 s), two tones.
fn four_seconds(dir: &Path) -> PathBuf {
    let path = dir.join("clip.mp4");
    make_clip(
        &path,
        &Spec {
            width: 640,
            height: 360,
            fps: 30,
            frames: 120,
            tones: vec![(440.0, 0.3), (1000.0, 0.3)],
            noise: 6,
        },
    );
    path
}

fn cut(start: f64, end: f64) -> Cut {
    Cut { start, end }
}

fn run<'a>(
    input: &'a Path,
    output: &'a Path,
    preset: ExportPreset,
    cut: Cut,
    gains: &'a [f32],
    overlay: Option<&'a mut dyn Overlay>,
) -> Result<ExportReport, MediaError> {
    let mut last = -1.0f32;
    let report = export(
        ExportJob {
            input,
            output,
            preset,
            cut,
            gains,
            overlay,
        },
        &mut |p| {
            assert!((0.0..=1.0).contains(&p), "progress {p}");
            last = p;
        },
        &AtomicBool::new(false),
    )?;
    assert_eq!(last, 1.0);
    assert_eq!(report.bytes, std::fs::metadata(output).unwrap().len());
    Ok(report)
}

fn share_preset(input: &Path, cut: Cut) -> ExportPreset {
    let rules = ShareSettings {
        max_bytes: 2_000_000,
        ..ShareSettings::default()
    };
    let plan = share::plan(
        &rules,
        probe(input).unwrap().source(),
        ClipKind::Regular,
        Some(cut),
    );
    ExportPreset::Share {
        plan,
        max_bytes: rules.max_bytes,
    }
}

fn gif(width: u32, fps: u32, max_bytes: u64) -> ExportPreset {
    ExportPreset::Gif(GifSettings {
        width,
        fps,
        max_bytes,
    })
}

/// Paints a solid red rectangle over the second quarter of the width and
/// the third quarter of the height, and remembers the times it was asked
/// for.
#[derive(Default)]
struct Red {
    times: Vec<f64>,
}

impl Overlay for Red {
    fn draw(&mut self, t: f64, frame: &mut RgbaImage) {
        self.times.push(t);
        let (w, h) = (frame.width as usize, frame.height as usize);
        for y in h / 2..3 * h / 4 {
            for x in w / 4..w / 2 {
                let i = (y * w + x) * 4;
                frame.data[i..i + 4].copy_from_slice(&[255, 0, 0, 255]);
            }
        }
    }

    fn is_empty(&self) -> bool {
        false
    }
}

/// Captions that never show.
struct Nothing;

impl Overlay for Nothing {
    fn draw(&mut self, _t: f64, _frame: &mut RgbaImage) {
        panic!("an empty overlay is never drawn");
    }

    fn is_empty(&self) -> bool {
        true
    }
}

/// Checks every frame is red in the middle of the rectangle and not red
/// outside it.
fn assert_red_rectangle(path: &Path) {
    let frames = rgba_frames(path);
    assert!(!frames.is_empty());
    for (n, f) in frames.iter().enumerate() {
        let [r, g, b, _] = f.pixel(3 * f.width / 8, 5 * f.height / 8);
        assert!(
            r > 180 && g < 90 && b < 90,
            "frame {n} of {}: ({r}, {g}, {b}) inside the rectangle",
            path.display()
        );
        let [r, g, b, _] = f.pixel(3 * f.width / 4, 5 * f.height / 8);
        assert!(
            !(r > 180 && g < 90 && b < 90),
            "frame {n}: red outside the rectangle"
        );
    }
}

fn leftovers(dir: &Path, keep: &[&Path]) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| !keep.contains(&p.as_path()))
        .collect()
}

#[test]
fn full_copies_the_stream_when_only_the_cut_changed() {
    let tmp = tempfile::tempdir().unwrap();
    let input = four_seconds(tmp.path());
    let output = tmp.path().join("full.mp4");

    let started = Instant::now();
    // Frame 30 is a keyframe; 1.01 s is within half a frame of it.
    let report = run(
        &input,
        &output,
        ExportPreset::Full,
        cut(1.01, 3.0),
        &[1.0, 1.0],
        Some(&mut Nothing),
    )
    .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(report.method, "stream copy");
    assert!(elapsed.as_secs_f64() < 2.0, "took {elapsed:?}");

    let source: Vec<_> = video_packets(&input)
        .into_iter()
        .filter(|p| p.pts >= 1.0 - 1e-6 && p.pts < 3.0 - 1e-6)
        .collect();
    let copied = video_packets(&output);
    assert_eq!(copied.len(), 60);
    assert_eq!(copied.len(), source.len());
    let sizes = |ps: &[support::PacketInfo]| ps.iter().map(|p| p.size).collect::<Vec<_>>();
    assert_eq!(sizes(&copied), sizes(&source), "packets changed");
    assert!(copied[0].key);
    let duration: f64 = copied.iter().map(|p| p.duration).sum();
    assert!((duration - 2.0).abs() < 1e-3, "{duration}");

    assert_eq!(frame_indices(&output), (30..90).collect::<Vec<_>>());
    // Both tracks, as they were.
    assert_eq!(audio_stream_count(&output), 2);
    let info = probe(&output).unwrap();
    assert!((info.duration - 2.0).abs() < 0.05, "{}", info.duration);
}

#[test]
fn full_reencodes_when_the_cut_starts_between_keyframes() {
    let tmp = tempfile::tempdir().unwrap();
    let input = four_seconds(tmp.path());
    let output = tmp.path().join("full.mp4");

    // Frame 33 isn't a keyframe.
    let report = run(
        &input,
        &output,
        ExportPreset::Full,
        cut(1.1, 3.0),
        &[],
        None,
    )
    .unwrap();
    assert!(
        report.method == "h264_videotoolbox" || report.method == "libx264 crf 18",
        "{}",
        report.method
    );
    let (src, out) = (probe(&input).unwrap(), probe(&output).unwrap());
    assert_eq!((out.width, out.height), (src.width, src.height));
    assert_eq!(out.fps, src.fps);
    assert!((out.duration - 1.9).abs() < 0.05, "{}", out.duration);
    assert_eq!(out.audio_tracks, 1, "mixed down");
    let frames = frame_indices(&output);
    assert_eq!(frames.len(), 57);
    assert_eq!(frames[0], 33);
    assert_eq!(*frames.last().unwrap(), 89);
}

#[test]
fn full_reencodes_when_a_gain_changed() {
    let tmp = tempfile::tempdir().unwrap();
    let input = four_seconds(tmp.path());
    let output = tmp.path().join("full.mp4");

    let report = run(
        &input,
        &output,
        ExportPreset::Full,
        cut(1.0, 3.0),
        &[1.0, 0.5],
        None,
    )
    .unwrap();
    assert_ne!(report.method, "stream copy");
    let out = probe(&output).unwrap();
    assert_eq!((out.width, out.height, out.fps), (640, 360, 30.0));
    assert_eq!(out.audio_tracks, 1);
    assert_eq!(frame_indices(&output)[0], 30);
}

#[test]
fn gif_has_the_asked_width_and_frame_rate() {
    let tmp = tempfile::tempdir().unwrap();
    let input = four_seconds(tmp.path());
    let output = tmp.path().join("clip.gif");

    let report = run(
        &input,
        &output,
        gif(160, 10, 10_000_000),
        cut(1.0, 3.0),
        &[],
        None,
    )
    .unwrap();
    assert_eq!(report.method, "gifski 160 px 10 fps");
    let frames = rgba_frames(&output);
    assert!((19..=21).contains(&frames.len()), "{} frames", frames.len());
    assert_eq!((frames[0].width, frames[0].height), (160, 90));
}

#[test]
fn gif_lowers_width_and_frame_rate_to_fit() {
    let tmp = tempfile::tempdir().unwrap();
    let input = four_seconds(tmp.path());
    let output = tmp.path().join("clip.gif");

    let first = run(
        &input,
        &output,
        gif(240, 20, 10_000_000),
        cut(1.0, 2.5),
        &[],
        None,
    )
    .unwrap();
    assert_eq!(first.method, "gifski 240 px 20 fps");
    std::fs::remove_file(&output).unwrap();

    let max_bytes = first.bytes / 4;
    let report = run(
        &input,
        &output,
        gif(240, 20, max_bytes),
        cut(1.0, 2.5),
        &[],
        None,
    )
    .unwrap();
    assert!(report.bytes <= max_bytes, "{} > {max_bytes}", report.bytes);
    let words: Vec<&str> = report.method.split(' ').collect();
    let (width, fps): (u32, u32) = (words[1].parse().unwrap(), words[3].parse().unwrap());
    assert!(width < 240 && fps < 20, "{}", report.method);
    assert!(fps >= 10);
    let frames = rgba_frames(&output);
    assert_eq!(frames[0].width, width);
    let expected = 1.5 * f64::from(fps);
    assert!(
        (frames.len() as f64 - expected).abs() <= 1.0,
        "{} frames at {fps} fps",
        frames.len()
    );
}

#[test]
fn gif_over_the_target_at_every_size_fails_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let input = four_seconds(tmp.path());
    let output = tmp.path().join("clip.gif");

    let err = run(
        &input,
        &output,
        gif(64, 10, 1_000),
        cut(1.0, 2.0),
        &[],
        None,
    )
    .unwrap_err();
    assert!(matches!(err, MediaError::OverBudget { .. }), "{err}");
    assert_eq!(leftovers(tmp.path(), &[&input]), Vec::<PathBuf>::new());
}

#[test]
fn captions_are_drawn_in_every_preset() {
    let tmp = tempfile::tempdir().unwrap();
    let input = four_seconds(tmp.path());
    let range = cut(1.0, 2.0);

    let mut red = Red::default();
    let output = tmp.path().join("share.mp4");
    let report = run(
        &input,
        &output,
        share_preset(&input, range),
        range,
        &[],
        Some(&mut red),
    )
    .unwrap();
    assert!(
        report.method.starts_with("libx264 two-pass"),
        "{}",
        report.method
    );
    assert_red_rectangle(&output);
    // Both passes, at the times of the original.
    assert_eq!(red.times.len(), 60);
    assert!((red.times[0] - 1.0).abs() < 1e-9 && (red.times[29] - 1.0 - 29.0 / 30.0).abs() < 1e-9);

    let mut red = Red::default();
    let output = tmp.path().join("full.mp4");
    let report = run(
        &input,
        &output,
        ExportPreset::Full,
        range,
        &[],
        Some(&mut red),
    )
    .unwrap();
    assert_ne!(report.method, "stream copy");
    assert_red_rectangle(&output);
    assert_eq!(red.times.len(), 30);

    let mut red = Red::default();
    let output = tmp.path().join("clip.gif");
    run(
        &input,
        &output,
        gif(160, 10, 10_000_000),
        range,
        &[],
        Some(&mut red),
    )
    .unwrap();
    assert_red_rectangle(&output);
    assert_eq!(red.times.len(), 10);
}

#[test]
fn cancelling_stops_and_leaves_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let input = four_seconds(tmp.path());
    let presets = [
        ("share.mp4", share_preset(&input, cut(0.0, 4.0))),
        ("full.mp4", ExportPreset::Full),
        ("clip.gif", gif(160, 10, 10_000_000)),
    ];
    for (name, preset) in presets {
        let output = tmp.path().join(name);
        let cancel = AtomicBool::new(false);
        let mut red = Red::default();
        let err = export(
            ExportJob {
                input: &input,
                output: &output,
                preset,
                cut: cut(0.5, 4.0),
                gains: &[],
                overlay: Some(&mut red),
            },
            &mut |p| {
                if p > 0.3 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(err, MediaError::Cancelled), "{name}: {err}");
        assert_eq!(
            leftovers(tmp.path(), &[&input]),
            Vec::<PathBuf>::new(),
            "{name}"
        );
    }
}

#[test]
fn estimates_are_near_the_real_sizes() {
    let tmp = tempfile::tempdir().unwrap();
    let input = four_seconds(tmp.path());
    let info = probe(&input).unwrap();
    assert!(info.bit_rate > 0);

    let check = |name: &str, preset: ExportPreset, range: Cut, gains: &[f32]| {
        let estimate = estimate_bytes(&info, &preset, range);
        let output = tmp.path().join(name);
        let report = run(&input, &output, preset, range, gains, None).unwrap();
        let ratio = estimate as f64 / report.bytes as f64;
        eprintln!(
            "{name}: estimate {estimate}, real {} ({}), ratio {ratio:.2}",
            report.bytes, report.method
        );
        assert!((0.3..=3.0).contains(&ratio), "{name}: ratio {ratio}");
    };
    check(
        "share.mp4",
        share_preset(&input, cut(0.0, 4.0)),
        cut(0.0, 4.0),
        &[],
    );
    check("copy.mp4", ExportPreset::Full, cut(1.0, 3.0), &[]);
    check("encode.mp4", ExportPreset::Full, cut(1.0, 3.0), &[0.5]);
    check("clip.gif", gif(160, 10, 10_000_000), cut(1.0, 3.0), &[]);
}
