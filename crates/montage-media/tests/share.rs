//! The Share export on clips generated at test time.

mod support;

use std::path::{Path, PathBuf};

use montage_core::share::{self, ClipKind, SharePlan, ShareSettings};
use montage_media::{MediaError, export_share, export_share_within, probe};
use support::{Spec, audio_samples, frame_indices, make_clip, tone_amplitude};

/// A budget small enough that a 6 s clip has to be trimmed: about 5.3 s at
/// 300 kbit/s of video and 64 of audio.
const TINY: ShareSettings = ShareSettings {
    max_bytes: 250_000,
    fps: 60,
    audio_kbps: 64,
    min_video_kbps: 300,
    hd_video_kbps: 100_000,
    max_video_kbps: 1_000,
};

fn clip(dir: &Path, name: &str, spec: &Spec) -> PathBuf {
    let path = dir.join(name);
    make_clip(&path, spec);
    path
}

fn six_seconds(tones: Vec<(f32, f32)>) -> Spec {
    Spec {
        width: 640,
        height: 360,
        fps: 30,
        frames: 180,
        tones,
        noise: 6,
    }
}

fn export(input: &Path, plan: &SharePlan, gains: &[f32]) -> PathBuf {
    let output = input.with_file_name(format!(
        "{}-share.mp4",
        input.file_stem().unwrap().to_string_lossy()
    ));
    let mut last = -1.0f32;
    export_share(input, &output, plan, gains, &mut |p| {
        assert!(p >= last, "progress went back from {last} to {p}");
        assert!((0.0..=1.0).contains(&p));
        last = p;
    })
    .unwrap();
    assert_eq!(last, 1.0);
    output
}

fn size(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

#[test]
fn probe_reads_the_clip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = clip(
        tmp.path(),
        "two.mp4",
        &Spec {
            width: 640,
            height: 360,
            fps: 30,
            frames: 90,
            tones: vec![(440.0, 0.3), (1000.0, 0.3)],
            noise: 6,
        },
    );
    let info = probe(&path).unwrap();
    assert!((info.duration - 3.0).abs() < 0.1, "{}", info.duration);
    assert_eq!((info.width, info.height), (640, 360));
    assert_eq!(info.fps, 30.0);
    assert_eq!(info.video_codec, "h264");
    assert_eq!(info.audio_tracks, 2);

    let silent = clip(
        tmp.path(),
        "silent.mp4",
        &Spec {
            tones: vec![],
            ..six_seconds(vec![])
        },
    );
    assert_eq!(probe(&silent).unwrap().audio_tracks, 0);
}

#[test]
fn a_replay_keeps_its_end_within_the_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let input = clip(tmp.path(), "Replay_x.mp4", &six_seconds(vec![(440.0, 0.3)]));
    let info = probe(&input).unwrap();
    let plan = share::plan(&TINY, info.source(), ClipKind::Replay, None);
    assert!(
        plan.range.start > 0.5,
        "the budget must force a trim: {plan:?}"
    );
    assert_eq!((plan.width, plan.height), (640, 360));

    let output = export(&input, &plan, &[]);
    eprintln!(
        "replay: {} bytes for a budget of {} ({:.1} s at {} kbit/s)",
        size(&output),
        TINY.max_bytes,
        plan.range.duration(),
        plan.video_kbps
    );
    assert!(size(&output) <= TINY.max_bytes);
    // Two-pass lands close to the budget, not far under it.
    assert!(size(&output) >= TINY.max_bytes * 85 / 100);

    let frames = frame_indices(&output);
    let expected = (plan.range.duration() * 30.0).round() as usize;
    assert!(
        frames.len().abs_diff(expected) <= 1,
        "{} frames, expected {expected}",
        frames.len()
    );
    let first = (plan.range.start * 30.0).floor() as i64;
    assert!(
        (i64::from(frames[0]) - first).abs() <= 1,
        "starts at frame {}, expected {first}",
        frames[0]
    );
    assert!(
        *frames.last().unwrap() >= 178,
        "ends at {:?}",
        frames.last()
    );
    // Frames come in order, each once: same frame rate in and out.
    assert!(frames.windows(2).all(|w| w[1] == w[0] + 1), "{frames:?}");

    let out = probe(&output).unwrap();
    assert!(
        (out.duration - plan.range.duration()).abs() < 0.1,
        "{} s, expected {}",
        out.duration,
        plan.range.duration()
    );
    assert_eq!(out.audio_tracks, 1);
    assert_eq!(out.video_codec, "h264");
}

#[test]
fn a_recording_keeps_its_start() {
    let tmp = tempfile::tempdir().unwrap();
    let input = clip(tmp.path(), "Video_x.mp4", &six_seconds(vec![(440.0, 0.3)]));
    let info = probe(&input).unwrap();
    let plan = share::plan(&TINY, info.source(), ClipKind::Regular, None);
    assert_eq!(plan.range.start, 0.0);
    assert!(plan.range.end < 5.5);

    let output = export(&input, &plan, &[]);
    eprintln!(
        "recording: {} bytes for a budget of {}",
        size(&output),
        TINY.max_bytes
    );
    assert!(size(&output) <= TINY.max_bytes);
    let frames = frame_indices(&output);
    assert_eq!(frames[0], 0);
    let last = (plan.range.end * 30.0).ceil() as i64 - 1;
    assert!((i64::from(*frames.last().unwrap()) - last).abs() <= 1);
}

#[test]
fn a_cut_in_the_middle_starts_on_the_right_frame() {
    let tmp = tempfile::tempdir().unwrap();
    let input = clip(tmp.path(), "cut.mp4", &six_seconds(vec![]));
    let info = probe(&input).unwrap();
    // 2.1 s is between keyframes (every 0.5 s): frames before it must be
    // decoded and dropped.
    let cut = montage_core::edit::Cut {
        start: 2.1,
        end: 4.0,
    };
    let plan = share::plan(
        &ShareSettings::default(),
        info.source(),
        ClipKind::Replay,
        Some(cut),
    );
    let output = export(&input, &plan, &[]);
    let frames = frame_indices(&output);
    assert_eq!(frames[0], 63);
    assert_eq!(frames.len(), 57);
    assert_eq!(*frames.last().unwrap(), 119);
}

#[test]
fn tracks_are_mixed_into_one_and_muting_removes_one() {
    let tmp = tempfile::tempdir().unwrap();
    let input = clip(
        tmp.path(),
        "two.mp4",
        &Spec {
            width: 320,
            height: 180,
            fps: 30,
            frames: 90,
            tones: vec![(440.0, 0.3), (1000.0, 0.3)],
            noise: 6,
        },
    );
    let info = probe(&input).unwrap();
    let plan = share::plan(
        &ShareSettings::default(),
        info.source(),
        ClipKind::Replay,
        None,
    );

    let both = export(&input, &plan, &[1.0, 1.0]);
    assert_eq!(probe(&both).unwrap().audio_tracks, 1);
    let (samples, rate) = audio_samples(&both);
    assert_eq!(rate, 48_000);
    let (a, b) = (
        tone_amplitude(&samples, rate, 440.0),
        tone_amplitude(&samples, rate, 1000.0),
    );
    eprintln!("both tracks: 440 Hz at {a:.3}, 1000 Hz at {b:.3}");
    assert!(a > 0.2 && b > 0.2);

    std::fs::remove_file(&both).unwrap();
    let muted = export(&input, &plan, &[1.0, 0.0]);
    let (samples, rate) = audio_samples(&muted);
    let (a, b) = (
        tone_amplitude(&samples, rate, 440.0),
        tone_amplitude(&samples, rate, 1000.0),
    );
    eprintln!("mic muted: 440 Hz at {a:.3}, 1000 Hz at {b:.3}");
    assert!(a > 0.2, "{a}");
    assert!(b < 0.01, "{b}");

    std::fs::remove_file(&muted).unwrap();
    let quieter = export(&input, &plan, &[0.5]);
    let (samples, rate) = audio_samples(&quieter);
    let (a, b) = (
        tone_amplitude(&samples, rate, 440.0),
        tone_amplitude(&samples, rate, 1000.0),
    );
    eprintln!("game at 50 %: 440 Hz at {a:.3}, 1000 Hz at {b:.3}");
    assert!((a / b - 0.5).abs() < 0.05, "{a} / {b}");
}

#[test]
fn the_edges_fade() {
    let tmp = tempfile::tempdir().unwrap();
    let input = clip(
        tmp.path(),
        "tone.mp4",
        &Spec {
            width: 320,
            height: 180,
            fps: 30,
            frames: 60,
            tones: vec![(440.0, 0.5)],
            noise: 6,
        },
    );
    let info = probe(&input).unwrap();
    let cut = montage_core::edit::Cut {
        start: 0.5,
        end: 1.5,
    };
    let plan = share::plan(
        &ShareSettings::default(),
        info.source(),
        ClipKind::Replay,
        Some(cut),
    );
    let output = export(&input, &plan, &[]);
    let (samples, _) = audio_samples(&output);
    let peak = |s: &[f32]| s.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    // AAC adds some delay and ringing; the first 5 ms must still be far
    // quieter than the middle.
    let middle = peak(&samples[samples.len() / 2 - 2400..samples.len() / 2 + 2400]);
    let head = peak(&samples[..240]);
    eprintln!("fade: head peak {head:.3}, middle peak {middle:.3}");
    assert!(middle > 0.4);
    assert!(head < middle / 4.0);
}

#[test]
fn a_clip_without_audio_exports_without_audio() {
    let tmp = tempfile::tempdir().unwrap();
    let input = clip(tmp.path(), "silent.mp4", &six_seconds(vec![]));
    let info = probe(&input).unwrap();
    let plan = share::plan(&TINY, info.source(), ClipKind::Replay, None);
    let output = export(&input, &plan, &[]);
    assert_eq!(probe(&output).unwrap().audio_tracks, 0);
    assert!(size(&output) <= TINY.max_bytes);
}

#[test]
fn the_frame_rate_is_capped() {
    let tmp = tempfile::tempdir().unwrap();
    let input = clip(
        tmp.path(),
        "fast.mp4",
        &Spec {
            width: 320,
            height: 180,
            fps: 50,
            frames: 100,
            tones: vec![],
            noise: 6,
        },
    );
    let rules = ShareSettings {
        fps: 25,
        ..ShareSettings::default()
    };
    let info = probe(&input).unwrap();
    let plan = share::plan(&rules, info.source(), ClipKind::Replay, None);
    let output = export(&input, &plan, &[]);
    assert_eq!(probe(&output).unwrap().fps, 25.0);
    let frames = frame_indices(&output);
    assert_eq!(frames.len(), 50);
    // Every other source frame.
    assert!(frames.windows(2).all(|w| w[1] == w[0] + 2), "{frames:?}");
}

#[test]
fn a_failed_export_leaves_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("missing.mp4");
    let output = tmp.path().join("missing-share.mp4");
    let plan = share::plan(
        &ShareSettings::default(),
        share::Source {
            duration: 5.0,
            width: 640,
            height: 360,
            fps: 30.0,
        },
        ClipKind::Replay,
        None,
    );
    let err = export_share(&input, &output, &plan, &[], &mut |_| {}).unwrap_err();
    assert!(matches!(err, MediaError::Ffmpeg { .. }), "{err}");
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
}

#[test]
fn an_overshoot_is_redone_at_a_lower_bitrate() {
    let tmp = tempfile::tempdir().unwrap();
    let input = clip(
        tmp.path(),
        "Replay_noisy.mp4",
        &Spec {
            noise: 40,
            ..six_seconds(vec![(440.0, 0.3)])
        },
    );
    let info = probe(&input).unwrap();
    let plan = share::plan(&TINY, info.source(), ClipKind::Replay, None);

    // On this content x264 overshoots its target…
    let once = export(&input, &plan, &[]);
    let first = size(&once);
    std::fs::remove_file(&once).unwrap();
    assert!(first > TINY.max_bytes, "{first}: the clip should overshoot");

    // …so the capped export retries until it fits.
    let output = tmp.path().join("capped.mp4");
    export_share_within(&input, &output, &plan, &[], TINY.max_bytes, &mut |_| {}).unwrap();
    eprintln!(
        "overshoot: {first} bytes at first, {} after retrying, for a budget of {}",
        size(&output),
        TINY.max_bytes
    );
    assert!(size(&output) <= TINY.max_bytes);
}
