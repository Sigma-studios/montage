//! The filmstrip, the waveform and the thumbnail, on clips made at test
//! time (each frame carries its index as a bar code, see `support`).

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use montage_core::render::RgbaImage;
use montage_media::{filmstrip, thumbnail, waveform};
use support::{BITS, Spec, make_clip};

const W: u32 = 320;
const H: u32 = 180;

/// A 320x180 clip at 30 fps with a keyframe every 15 frames.
fn clip(dir: &Path, name: &str, frames: i64, tones: Vec<(f32, f32)>) -> PathBuf {
    let path = dir.join(name);
    make_clip(
        &path,
        &Spec {
            width: W,
            height: H,
            fps: 30,
            frames,
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

#[test]
fn filmstrip_spreads_keyframes_over_the_clip() {
    let dir = tempfile::tempdir().unwrap();
    let path = clip(dir.path(), "a.mp4", 90, vec![]);
    let images = filmstrip(&path, 6, 90, None).unwrap();
    assert_eq!(images.len(), 6);
    for image in &images {
        assert_eq!((image.width, image.height), (160, 90));
        assert_eq!(image.data.len(), 160 * 90 * 4);
    }
    // Slots at 0.25 s, 0.75 s…: the keyframe before each (every 15 frames).
    let indices: Vec<u32> = images.iter().map(bars).collect();
    assert_eq!(indices, [0, 15, 30, 45, 60, 75]);

    // More slots than keyframes: neighbours share one.
    let images = filmstrip(&path, 12, 45, None).unwrap();
    assert_eq!(images.len(), 12);
    assert!(images.iter().all(|i| (i.width, i.height) == (80, 45)));
    assert!(filmstrip(&path, 0, 45, None).unwrap().is_empty());
}

#[test]
fn thumbnail_is_a_third_in() {
    let dir = tempfile::tempdir().unwrap();
    let path = clip(dir.path(), "a.mp4", 90, vec![(440.0, 0.5)]);
    let image = thumbnail(&path, 72, None).unwrap();
    assert_eq!((image.width, image.height), (128, 72));
    assert_eq!(bars(&image), 30);
}

#[test]
fn waveform_has_peaks_per_track() {
    let dir = tempfile::tempdir().unwrap();
    let path = clip(dir.path(), "a.mp4", 60, vec![(440.0, 0.0), (440.0, 0.8)]);
    let wave = waveform(&path, 40, None).unwrap();
    assert_eq!(wave.tracks.len(), 2);
    assert!(
        (wave.bucket_seconds - 2.0 / 40.0).abs() < 0.005,
        "{}",
        wave.bucket_seconds
    );
    for track in &wave.tracks {
        assert_eq!(track.len(), 40);
    }
    let silent = &wave.tracks[0];
    assert!(
        silent
            .iter()
            .all(|&(lo, hi)| lo.abs() < 0.01 && hi.abs() < 0.01)
    );
    let loud = &wave.tracks[1];
    // Away from the encoder's fade-in at the start.
    for &(lo, hi) in &loud[2..38] {
        assert!((0.65..=1.0).contains(&hi), "{loud:?}");
        assert!((-1.0..=-0.65).contains(&lo), "{loud:?}");
    }

    let muted = wave.mixed(&[1.0, 0.0]);
    assert!(
        muted
            .iter()
            .all(|&(lo, hi)| lo.abs() < 0.01 && hi.abs() < 0.01)
    );
    let mixed = wave.mixed(&[]);
    assert!(mixed[20].1 > 0.65);
}

#[test]
fn waveform_without_audio_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = clip(dir.path(), "a.mp4", 30, vec![]);
    let wave = waveform(&path, 10, None).unwrap();
    assert!(wave.tracks.is_empty());
    assert!(wave.mixed(&[]).is_empty());
}

fn cache_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    files
}

#[test]
fn caches_until_the_clip_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = clip(dir.path(), "a.mp4", 90, vec![(440.0, 0.5)]);
    let cache = dir.path().join(".thumbs");

    let strip = filmstrip(&path, 6, 90, Some(&cache)).unwrap();
    let wave = waveform(&path, 30, Some(&cache)).unwrap();
    let thumb = thumbnail(&path, 72, Some(&cache)).unwrap();
    assert_eq!(cache_files(&cache).len(), 3, "{:?}", cache_files(&cache));

    assert_eq!(filmstrip(&path, 6, 90, Some(&cache)).unwrap(), strip);
    assert_eq!(waveform(&path, 30, Some(&cache)).unwrap(), wave);
    assert_eq!(thumbnail(&path, 72, Some(&cache)).unwrap(), thumb);
    assert_eq!(filmstrip(&path, 6, 90, None).unwrap(), strip);

    // Other parameters get their own entry.
    let other = filmstrip(&path, 3, 90, Some(&cache)).unwrap();
    assert_eq!(other.len(), 3);
    assert_eq!(cache_files(&cache).len(), 4);

    // The cache is what's read: a pixel changed in it comes back.
    let strip_file = cache_files(&cache)
        .into_iter()
        .find(|f| f.to_string_lossy().ends_with("filmstrip-6x90.bin"))
        .unwrap();
    let mut data = fs::read(&strip_file).unwrap();
    let last = data.len() - 2;
    data[last] ^= 0xff;
    fs::write(&strip_file, &data).unwrap();
    let tampered = filmstrip(&path, 6, 90, Some(&cache)).unwrap();
    assert_ne!(tampered, strip);
    assert_eq!(tampered[..5], strip[..5]);

    // A truncated cache is made again.
    fs::write(&strip_file, &data[..100]).unwrap();
    assert_eq!(filmstrip(&path, 6, 90, Some(&cache)).unwrap(), strip);

    // A new clip under the same name is made again.
    clip(dir.path(), "a.mp4", 60, vec![(440.0, 0.5), (220.0, 0.5)]);
    let strip2 = filmstrip(&path, 6, 90, Some(&cache)).unwrap();
    let indices: Vec<u32> = strip2.iter().map(bars).collect();
    assert_eq!(indices, [0, 15, 15, 30, 45, 45]);
    let wave2 = waveform(&path, 30, Some(&cache)).unwrap();
    assert_eq!(wave2.tracks.len(), 2);
    assert_ne!(thumbnail(&path, 72, Some(&cache)).unwrap(), thumb);
    assert_eq!(cache_files(&cache).len(), 4, "entries are replaced");
}

/// Timings on a real clip: `MONTAGE_BENCH_CLIP=clip.mp4 cargo test --test
/// thumbs -- --ignored --nocapture`.
#[test]
#[ignore]
fn bench_on_a_real_clip() {
    let Ok(path) = std::env::var("MONTAGE_BENCH_CLIP") else {
        return;
    };
    let path = PathBuf::from(path);
    let t = std::time::Instant::now();
    let strip = filmstrip(&path, 30, 72, None).unwrap();
    eprintln!("filmstrip 30 x 72 px: {:?}", t.elapsed());
    assert_eq!(strip.len(), 30);
    let t = std::time::Instant::now();
    let wave = waveform(&path, 1000, None).unwrap();
    eprintln!("waveform, {} tracks: {:?}", wave.tracks.len(), t.elapsed());
    let t = std::time::Instant::now();
    thumbnail(&path, 180, None).unwrap();
    eprintln!("thumbnail: {:?}", t.elapsed());
}
