//! The player on clips made at test time: each frame carries its index as a
//! bar code (see `support`), so a test can tell which frame it got.

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use montage_core::render::RgbaImage;
use montage_media::{Player, VideoFrame};
use support::{BITS, Spec, make_clip};

const W: u32 = 320;
const H: u32 = 180;
const FPS: i32 = 30;
const FRAMES: i64 = 90;

fn clip(dir: &Path, name: &str, tones: Vec<(f32, f32)>) -> PathBuf {
    let path = dir.join(name);
    make_clip(
        &path,
        &Spec {
            width: W,
            height: H,
            fps: FPS,
            frames: FRAMES,
            tones,
            noise: 1,
        },
    );
    path
}

/// The bar code of a frame scaled from `W` wide.
fn bars(image: &RgbaImage) -> u32 {
    let scale = f64::from(image.width) / f64::from(W);
    let block = W / BITS;
    let (y0, y1) = (image.height / 16, 3 * image.height / 16);
    let mut value = 0;
    for bit in 0..BITS {
        let x0 = (f64::from(bit * block + block / 4) * scale) as u32;
        let x1 = (f64::from(bit * block + 3 * block / 4) * scale) as u32;
        let (mut sum, mut n) = (0u64, 0u64);
        for y in y0..y1 {
            for x in x0..x1.max(x0 + 1) {
                sum += u64::from(image.data[((y * image.width + x) * 4) as usize]);
                n += 1;
            }
        }
        value = value << 1 | u32::from(sum / n > 128);
    }
    value
}

/// Waits for the next frame.
fn next_frame(player: &mut Player) -> VideoFrame {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(f) = player.take_frame() {
            return f;
        }
        assert!(Instant::now() < deadline, "no frame came");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn frame_index(f: &VideoFrame) -> u32 {
    bars(&f.image)
}

/// Seconds of frame `i`.
fn at(i: u32) -> f64 {
    f64::from(i) / f64::from(FPS)
}

#[test]
fn opens_paused_on_the_first_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = clip(dir.path(), "a.mp4", vec![(440.0, 0.5)]);
    let mut player = Player::open(&path, 160, 160).unwrap();

    let info = player.info().clone();
    assert_eq!((info.width, info.height), (W, H));
    assert_eq!(info.fps, 30.0);
    assert_eq!(info.audio_tracks, 1);
    assert!((info.duration - 3.0).abs() < 0.1, "{}", info.duration);
    let name = player.decoder_name();
    assert!(name.starts_with("h264 ("), "{name}");
    assert!(!player.is_playing());

    let f = next_frame(&mut player);
    assert_eq!((f.image.width, f.image.height), (160, 90));
    assert_eq!(f.image.data.len(), 160 * 90 * 4);
    assert_eq!(f.pts, 0.0);
    assert_eq!(frame_index(&f), 0);
    assert_eq!(player.position(), 0.0);
    assert!(player.take_frame().is_none(), "the frame didn't change");
}

#[test]
fn never_scales_up() {
    let dir = tempfile::tempdir().unwrap();
    let path = clip(dir.path(), "a.mp4", vec![]);
    let mut player = Player::open(&path, 1920, 1080).unwrap();
    let f = next_frame(&mut player);
    assert_eq!((f.image.width, f.image.height), (W, H));
}

#[test]
fn seeks_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let path = clip(dir.path(), "a.mp4", vec![]);
    let mut player = Player::open(&path, W, H).unwrap();
    next_frame(&mut player);

    // Forwards, backwards, across keyframes (every 15 frames), on and
    // between frame times.
    for (t, want) in [
        (at(37), 37),
        (at(5) + 0.01, 5),
        (at(61) + 0.03, 61),
        (at(14), 14),
        (at(15), 15),
        (at(89), 89),
        (at(44) + 0.02, 44),
        (10.0, 89),
        (0.0, 0),
    ] {
        player.seek(t);
        let f = next_frame(&mut player);
        assert_eq!(frame_index(&f), want, "seek to {t}");
        assert!(
            (f.pts - at(want)).abs() < 1e-3,
            "seek to {t}: pts {}",
            f.pts
        );
        assert!((player.position() - t.min(3.0)).abs() < 1e-6);
    }
}

#[test]
fn steps_frame_by_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = clip(dir.path(), "a.mp4", vec![]);
    let mut player = Player::open(&path, 160, 90).unwrap();
    next_frame(&mut player);
    player.seek(at(20));
    assert_eq!(frame_index(&next_frame(&mut player)), 20);

    for (step, want) in [
        (1, 21),
        (1, 22),
        (-1, 21),
        (-3, 18),
        (-5, 13),
        (-5, 8),
        (10, 18),
        (-1, 17),
        (-20, 0),
        (-1, 0),
    ] {
        player.step(step);
        let f = if step == -1 && want == 0 {
            // Already on the first frame: nothing changes.
            std::thread::sleep(Duration::from_millis(50));
            assert!(player.take_frame().is_none());
            continue;
        } else {
            next_frame(&mut player)
        };
        assert_eq!(frame_index(&f), want, "step {step}");
        assert!(!player.is_playing());
        assert!(
            (player.position() - at(want)).abs() < 1e-3,
            "step {step}: position {}",
            player.position()
        );
    }

    // Stepping while playing pauses.
    player.play();
    player.step(1);
    assert!(!player.is_playing());
}

#[test]
fn scrubbing_lands_on_the_last_seek() {
    let dir = tempfile::tempdir().unwrap();
    let path = clip(dir.path(), "a.mp4", vec![]);
    let mut player = Player::open(&path, 160, 90).unwrap();
    next_frame(&mut player);

    // Dragging the playhead: a seek per UI frame, without waiting for the
    // frames, forwards then backwards, with the frames taken now and then.
    for (from, to, step) in [(0i32, 80i32, 1i32), (80, 10, -3), (10, 47, 2)] {
        let mut i = from;
        while (step > 0 && i < to) || (step < 0 && i > to) {
            player.seek(at(i as u32) + 0.005);
            if i % 4 == 0 {
                let _ = player.take_frame();
            }
            std::thread::sleep(Duration::from_millis(1));
            i += step;
        }
        player.seek(at(to as u32) + 0.005);
        // The last frame that comes is the one at the last seek.
        let mut last = next_frame(&mut player);
        let settle = Instant::now();
        while settle.elapsed() < Duration::from_millis(150) {
            if let Some(f) = player.take_frame() {
                last = f;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(frame_index(&last), to as u32, "scrub to {to}");
        assert!((player.position() - (at(to as u32) + 0.005)).abs() < 1e-6);
    }
}

/// Plays for `secs`, taking frames as a UI would, and returns them with
/// the positions seen.
fn play_for(player: &mut Player, secs: f64) -> (Vec<(f64, u32)>, Vec<f64>) {
    let mut frames = Vec::new();
    let mut positions = Vec::new();
    let start = Instant::now();
    while start.elapsed().as_secs_f64() < secs {
        if let Some(f) = player.take_frame() {
            frames.push((f.pts, frame_index(&f)));
        }
        positions.push(player.position());
        std::thread::sleep(Duration::from_millis(4));
    }
    (frames, positions)
}

fn check_playback(player: &mut Player) {
    next_frame(player);
    player.play();
    assert!(player.is_playing());
    let (frames, positions) = play_for(player, 0.6);
    let pos = player.position();
    // The audio clock starts once the device asks for samples, so allow
    // for its start-up.
    assert!((0.35..0.75).contains(&pos), "position {pos}");
    assert!(positions.windows(2).all(|w| w[1] >= w[0]), "{positions:?}");
    assert!(frames.len() >= 6, "{frames:?}");
    for w in frames.windows(2) {
        assert!(w[1].0 > w[0].0, "{frames:?}");
    }
    for &(pts, index) in &frames {
        assert_eq!(index, (pts * 30.0).round() as u32);
    }
    let last = frames.last().unwrap().1;
    assert!((8..=23).contains(&last), "{frames:?}");

    player.pause();
    assert!(!player.is_playing());
    let paused = player.position();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(player.position(), paused);
}

#[test]
fn plays_on_the_wall_clock_without_audio() {
    let dir = tempfile::tempdir().unwrap();
    let path = clip(dir.path(), "a.mp4", vec![]);
    let mut player = Player::open(&path, 160, 90).unwrap();
    assert!(!player.has_audio_output());
    check_playback(&mut player);
}

#[test]
fn plays_with_audio() {
    let dir = tempfile::tempdir().unwrap();
    let path = clip(dir.path(), "a.mp4", vec![(440.0, 0.5), (880.0, 0.5)]);
    let mut player = Player::open(&path, 160, 90).unwrap();
    // Silent on the machine running the tests; the clock runs all the
    // same. Without an output device, the wall clock takes over.
    player.set_gains(&[0.0, 0.0]);
    eprintln!("audio output: {}", player.has_audio_output());
    check_playback(&mut player);
}

#[test]
fn plays_faster_and_backwards() {
    let dir = tempfile::tempdir().unwrap();
    let path = clip(dir.path(), "a.mp4", vec![(440.0, 0.5)]);
    let mut player = Player::open(&path, 160, 90).unwrap();
    player.set_gains(&[0.0]);
    next_frame(&mut player);

    player.set_speed(2.0);
    player.play();
    play_for(&mut player, 0.4);
    let pos = player.position();
    assert!((0.6..1.0).contains(&pos), "2x: {pos}");

    player.seek(2.5);
    player.set_speed(-1.0);
    let (frames, _) = play_for(&mut player, 0.5);
    let pos = player.position();
    assert!((1.85..2.15).contains(&pos), "-1x: {pos}");
    assert!(player.is_playing());
    let last = frames.last().unwrap();
    assert!((54..75).contains(&last.1), "{frames:?}");
    for w in frames.windows(2) {
        assert!(w[1].0 < w[0].0, "{frames:?}");
    }

    // Backwards to the start, where it stops.
    player.seek(0.2);
    play_for(&mut player, 0.4);
    assert!(!player.is_playing());
    assert_eq!(player.position(), 0.0);

    player.set_speed(0.0);
    assert!(!player.is_playing());
}

#[test]
fn loops_over_the_range() {
    let dir = tempfile::tempdir().unwrap();
    let path = clip(dir.path(), "a.mp4", vec![]);
    let mut player = Player::open(&path, 160, 90).unwrap();
    next_frame(&mut player);

    player.set_loop(Some((1.0, 1.4)));
    // Outside the range: playing starts at its start.
    player.seek(2.5);
    player.play();
    let (frames, positions) = play_for(&mut player, 1.1);
    assert!(player.is_playing());
    let wraps = positions.windows(2).filter(|w| w[1] < w[0]).count();
    assert!(wraps >= 2, "{positions:?}");
    for &p in &positions[2..] {
        assert!((1.0..=1.45).contains(&p), "{positions:?}");
    }
    for &(_, index) in &frames[1..] {
        assert!((30..=42).contains(&index), "{frames:?}");
    }

    // Without the loop, it plays on and stops at the end.
    player.set_loop(None);
    player.seek(2.8);
    let (frames, _) = play_for(&mut player, 0.5);
    assert!(!player.is_playing());
    assert!((player.position() - 3.0).abs() < 0.05);
    assert_eq!(frames.last().unwrap().1, 89);

    // Playing again from the end starts over.
    player.play();
    let f = next_frame(&mut player);
    assert!(frame_index(&f) < 10);
}

#[test]
fn resizes_the_preview() {
    let dir = tempfile::tempdir().unwrap();
    let path = clip(dir.path(), "a.mp4", vec![]);
    let mut player = Player::open(&path, 160, 90).unwrap();
    next_frame(&mut player);
    player.seek(at(40));
    assert_eq!(frame_index(&next_frame(&mut player)), 40);

    player.set_preview_size(240, 240);
    let f = next_frame(&mut player);
    assert_eq!((f.image.width, f.image.height), (240, 135));
    assert_eq!(frame_index(&f), 40);
    assert!((player.position() - at(40)).abs() < 1e-6);
}

/// Timings on a real clip: `MONTAGE_BENCH_CLIP=clip.mp4 cargo test --test
/// player -- --ignored --nocapture`.
#[test]
#[ignore]
fn bench_on_a_real_clip() {
    let Ok(path) = std::env::var("MONTAGE_BENCH_CLIP") else {
        return;
    };
    let t = Instant::now();
    let mut player = Player::open(Path::new(&path), 1280, 720).unwrap();
    player.set_gains(&[0.0; 8]);
    next_frame(&mut player);
    eprintln!(
        "{}, audio {}: first frame {:?}",
        player.decoder_name(),
        player.has_audio_output(),
        t.elapsed()
    );
    let fps = player.info().fps;
    for target in [7.3, 2.1, 15.9, 15.95, 0.5] {
        let t = Instant::now();
        player.seek(target);
        let f = next_frame(&mut player);
        eprintln!("seek to {target}: {:?} (pts {:.3})", t.elapsed(), f.pts);
    }
    let t = Instant::now();
    for _ in 0..10 {
        player.step(-1);
        next_frame(&mut player);
    }
    eprintln!("10 steps back: {:?}", t.elapsed());
    player.seek(1.0);
    next_frame(&mut player);
    player.play();
    let (frames, _) = play_for(&mut player, 3.0);
    let pos = player.position();
    eprintln!(
        "played 3 s: {} frames shown of {:.0}, position {pos:.3}",
        frames.len(),
        3.0 * fps
    );
}

#[test]
fn can_live_on_any_thread() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Player>();
}
