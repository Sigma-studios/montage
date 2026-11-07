//! The filmstrip, the waveform and the library's thumbnail, cached in the
//! clip's `.thumbs/` folder so they're made once.
//!
//! The filmstrip and the thumbnail use the keyframe at or before each time
//! (exact frames don't matter there, and a keyframe decodes alone), on a
//! few threads at once. The waveform decodes the audio once.
//!
//! Cache files are named after the clip and the parameters
//! (`clip.mp4.filmstrip-24x72.bin`) and start with the clip's size and
//! modification time, so a changed clip is redone. A cache that can't be
//! written is only a missed cache: the result is still returned.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use ffmpeg_next::software::scaling;
use montage_core::render::RgbaImage;

use crate::decode::{
    AudioTrack, Threads, ToRgba, VideoSource, discard_others, read_packet, start_offset,
};
use crate::{Context, MediaError};

/// `count` frames evenly spread over the clip, each `height` pixels tall.
/// Cached in `cache_dir` when given.
pub fn filmstrip(
    path: &Path,
    count: usize,
    height: u32,
    cache_dir: Option<&Path>,
) -> Result<Vec<RgbaImage>, MediaError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let key = format!("filmstrip-{count}x{height}");
    cached(path, cache_dir, &key, read_images, write_images, || {
        let duration = crate::probe(path)?.duration;
        let times: Vec<f64> = (0..count)
            .map(|i| (i as f64 + 0.5) * duration / count as f64)
            .collect();
        frames_at(path, &times, height)
    })
}

/// Peaks of the audio, per track.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Waveform {
    /// Seconds each bucket covers.
    pub bucket_seconds: f64,
    /// For each audio track, the (min, max) sample in each bucket, in -1..=1.
    pub tracks: Vec<Vec<(f32, f32)>>,
}

impl Waveform {
    /// The tracks mixed with `gains` (missing entries 1.0), as the export
    /// would: what the timeline draws.
    pub fn mixed(&self, gains: &[f32]) -> Vec<(f32, f32)> {
        let len = self.tracks.iter().map(Vec::len).max().unwrap_or(0);
        (0..len)
            .map(|i| {
                let (mut lo, mut hi) = (0.0f32, 0.0f32);
                for (t, track) in self.tracks.iter().enumerate() {
                    let g = gains.get(t).copied().unwrap_or(1.0);
                    if let Some(&(min, max)) = track.get(i) {
                        lo += min * g;
                        hi += max * g;
                    }
                }
                (lo.clamp(-1.0, 1.0), hi.clamp(-1.0, 1.0))
            })
            .collect()
    }
}

/// The clip's waveform in `buckets` buckets. Cached in `cache_dir` when
/// given.
pub fn waveform(
    path: &Path,
    buckets: usize,
    cache_dir: Option<&Path>,
) -> Result<Waveform, MediaError> {
    let key = format!("waveform-{buckets}");
    cached(path, cache_dir, &key, read_waveform, write_waveform, || {
        make_waveform(path, buckets)
    })
}

/// The library tile's thumbnail: a frame from about a third in, `height`
/// pixels tall. Cached in `cache_dir` when given.
pub fn thumbnail(
    path: &Path,
    height: u32,
    cache_dir: Option<&Path>,
) -> Result<RgbaImage, MediaError> {
    let key = format!("thumbnail-{height}");
    let images = cached(path, cache_dir, &key, read_images, write_images, || {
        let duration = crate::probe(path)?.duration;
        frames_at(path, &[duration / 3.0], height)
    })?;
    Ok(images.into_iter().next().unwrap_or_default())
}

/// The frame at (the keyframe before) each of `times`, `height` pixels
/// tall, decoded on several threads.
fn frames_at(path: &Path, times: &[f64], height: u32) -> Result<Vec<RgbaImage>, MediaError> {
    let height = height.max(1);
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 8)
        .min(times.len());
    let per = times.len().div_ceil(threads.max(1));
    let results: Vec<Result<Vec<RgbaImage>, MediaError>> = std::thread::scope(|s| {
        let handles: Vec<_> = times
            .chunks(per.max(1))
            .map(|chunk| s.spawn(move || frames_one_thread(path, chunk, height)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("the thumbnail thread doesn't panic"))
            .collect()
    });
    let mut images = Vec::with_capacity(times.len());
    for r in results {
        images.extend(r?);
    }
    Ok(images)
}

fn frames_one_thread(
    path: &Path,
    times: &[f64],
    height: u32,
) -> Result<Vec<RgbaImage>, MediaError> {
    let mut source = VideoSource::open(path, false, Threads::One)?;
    let width = if source.height > 0 {
        ((f64::from(height) * f64::from(source.width) / f64::from(source.height)).round() as u32)
            .max(1)
    } else {
        height
    };
    let mut rgba = ToRgba::new(scaling::Flags::AREA);
    let mut images: Vec<RgbaImage> = Vec::with_capacity(times.len());
    // The last keyframe decoded, by pts: neighbouring slots often share it.
    let mut last: Option<(Option<f64>, usize)> = None;
    for &t in times {
        source.seek(t);
        let mut keyframe = None;
        for _ in 0..500 {
            match source.next_packet() {
                Some(p) if p.is_key() => {
                    keyframe = Some(p);
                    break;
                }
                Some(_) => {}
                None => break,
            }
        }
        if let Some(packet) = &keyframe {
            let pts = source.packet_pts(packet);
            if let Some((last_pts, i)) = last
                && last_pts == pts
            {
                images.push(images[i].clone());
                continue;
            }
            last = Some((pts, images.len()));
        }
        let frame = match &keyframe {
            Some(packet) => source.decode_alone(packet)?,
            None => None,
        };
        // A keyframe that didn't decode alone: decode from the seek point.
        let frame = match frame {
            Some(f) => Some(f),
            None => {
                source.seek(t);
                source.next_frame(|_| true, true)?.and_then(|d| d.frame)
            }
        };
        let image = match frame {
            Some(f) => rgba.convert(&f, width, height)?,
            None => images
                .last()
                .cloned()
                .unwrap_or_else(|| black(width, height)),
        };
        images.push(image);
    }
    Ok(images)
}

fn black(width: u32, height: u32) -> RgbaImage {
    let mut image = RgbaImage::new(width, height);
    for px in image.data.chunks_exact_mut(4) {
        px[3] = 255;
    }
    image
}

fn make_waveform(path: &Path, buckets: usize) -> Result<Waveform, MediaError> {
    crate::init();
    let mut ictx =
        ffmpeg_next::format::input(path).context(|| format!("can't open {}", path.display()))?;
    let offset = start_offset(&ictx);
    let duration = if ictx.duration() > 0 {
        ictx.duration() as f64 / f64::from(ffmpeg_next::ffi::AV_TIME_BASE)
    } else {
        crate::probe(path)?.duration
    };
    let mut tracks = AudioTrack::open_all(&ictx, 0)?;
    let streams: Vec<usize> = tracks.iter().map(|t| t.stream).collect();
    discard_others(&mut ictx, &streams);

    let bucket_seconds = if buckets > 0 {
        duration / buckets as f64
    } else {
        duration
    };
    let empty = (f32::INFINITY, f32::NEG_INFINITY);
    let mut peaks = vec![vec![empty; buckets]; tracks.len()];
    // Where each track's next sample goes, in seconds.
    let mut next: Vec<Option<f64>> = vec![None; tracks.len()];

    let mut add = |n: usize, at: Option<f64>, rate: u32, l: &[f32], r: &[f32]| {
        if buckets == 0 || bucket_seconds <= 0.0 {
            return;
        }
        let rate = f64::from(rate);
        let at = at.map(|s| s - offset);
        let start = match (next[n], at) {
            (Some(next), Some(at)) if (next - at).abs() > 0.1 => at,
            (Some(next), _) => next,
            (None, Some(at)) => at,
            (None, None) => 0.0,
        };
        next[n] = Some(start + l.len() as f64 / rate);
        let track = &mut peaks[n];
        for (i, (a, b)) in l.iter().zip(r).enumerate() {
            let t = start + i as f64 / rate;
            if t < 0.0 {
                continue;
            }
            let bucket = ((t / bucket_seconds) as usize).min(buckets - 1);
            let (lo, hi) = &mut track[bucket];
            *lo = lo.min(*a).min(*b);
            *hi = hi.max(*a).max(*b);
        }
    };

    while let Some(packet) = read_packet(&mut ictx) {
        let Some(n) = tracks.iter().position(|t| t.stream == packet.stream()) else {
            continue;
        };
        if tracks[n].decoder.send_packet(&packet).is_err() {
            continue;
        }
        tracks[n].drain(&mut |at, rate, l, r| add(n, at, rate, l, r))?;
    }
    for (n, track) in tracks.iter_mut().enumerate() {
        let _ = track.decoder.send_eof();
        track.drain(&mut |at, rate, l, r| add(n, at, rate, l, r))?;
    }

    let tracks = peaks
        .into_iter()
        .map(|track| {
            track
                .into_iter()
                .map(|(lo, hi)| {
                    if lo > hi {
                        (0.0, 0.0)
                    } else {
                        (lo.clamp(-1.0, 1.0), hi.clamp(-1.0, 1.0))
                    }
                })
                .collect()
        })
        .collect();
    Ok(Waveform {
        bucket_seconds,
        tracks,
    })
}

// The cache.

const MAGIC: &[u8; 8] = b"MTGTHMB1";

/// Returns the cached result for `key` when it's there and the clip
/// hasn't changed, else makes it and caches it.
fn cached<T>(
    path: &Path,
    cache_dir: Option<&Path>,
    key: &str,
    read: fn(&mut Reader) -> Option<T>,
    write: fn(&T, &mut Vec<u8>),
    make: impl FnOnce() -> Result<T, MediaError>,
) -> Result<T, MediaError> {
    let Some(dir) = cache_dir else {
        return make();
    };
    let stamp = stamp(path)?;
    let file = cache_file(dir, path, key);
    if let Ok(data) = fs::read(&file)
        && data.len() >= MAGIC.len() + 20
        && &data[..MAGIC.len()] == MAGIC
        && data[MAGIC.len()..MAGIC.len() + 20] == stamp
    {
        let mut reader = Reader(&data[MAGIC.len() + 20..]);
        if let Some(value) = read(&mut reader)
            && reader.0.is_empty()
        {
            return Ok(value);
        }
    }
    let value = make()?;
    let mut data = Vec::new();
    data.extend_from_slice(MAGIC);
    data.extend_from_slice(&stamp);
    write(&value, &mut data);
    let _ = store(dir, &file, &data);
    Ok(value)
}

/// The clip's size and modification time, as stored in the cache.
fn stamp(path: &Path) -> Result<[u8; 20], MediaError> {
    let meta = fs::metadata(path).map_err(|source| MediaError::Io {
        action: "can't read",
        path: path.into(),
        source,
    })?;
    let since_epoch = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .unwrap_or_default();
    let mut stamp = [0u8; 20];
    stamp[..8].copy_from_slice(&meta.len().to_le_bytes());
    stamp[8..16].copy_from_slice(&since_epoch.as_secs().to_le_bytes());
    stamp[16..].copy_from_slice(&since_epoch.subsec_nanos().to_le_bytes());
    Ok(stamp)
}

fn cache_file(dir: &Path, path: &Path, key: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clip".into());
    dir.join(format!("{name}.{key}.bin"))
}

/// Writes `data` to `file` through a temporary file, so a reader never sees
/// half of it.
fn store(dir: &Path, file: &Path, data: &[u8]) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = file.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, data)?;
    fs::rename(&tmp, file).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn bytes(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }

    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }

    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
    }
}

fn read_images(r: &mut Reader) -> Option<Vec<RgbaImage>> {
    let count = r.u32()? as usize;
    let mut images = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let (width, height) = (r.u32()?, r.u32()?);
        let len = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        let data = r.bytes(len)?.to_vec();
        images.push(RgbaImage {
            width,
            height,
            data,
        });
    }
    Some(images)
}

fn write_images(images: &Vec<RgbaImage>, out: &mut Vec<u8>) {
    out.extend_from_slice(&(images.len() as u32).to_le_bytes());
    for image in images {
        out.extend_from_slice(&image.width.to_le_bytes());
        out.extend_from_slice(&image.height.to_le_bytes());
        out.extend_from_slice(&image.data);
    }
}

fn read_waveform(r: &mut Reader) -> Option<Waveform> {
    let bucket_seconds = r.f64()?;
    let count = r.u32()? as usize;
    let buckets = r.u32()? as usize;
    let mut tracks = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let mut track = Vec::with_capacity(buckets.min(1 << 20));
        for _ in 0..buckets {
            track.push((r.f32()?, r.f32()?));
        }
        tracks.push(track);
    }
    Some(Waveform {
        bucket_seconds,
        tracks,
    })
}

fn write_waveform(waveform: &Waveform, out: &mut Vec<u8>) {
    let buckets = waveform.tracks.first().map_or(0, Vec::len);
    out.extend_from_slice(&waveform.bucket_seconds.to_le_bytes());
    out.extend_from_slice(&(waveform.tracks.len() as u32).to_le_bytes());
    out.extend_from_slice(&(buckets as u32).to_le_bytes());
    for track in &waveform.tracks {
        for &(lo, hi) in track {
            out.extend_from_slice(&lo.to_le_bytes());
            out.extend_from_slice(&hi.to_le_bytes());
        }
    }
}
