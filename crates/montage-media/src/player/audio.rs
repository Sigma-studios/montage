//! The player's audio: every track decoded on one thread, resampled to the
//! output's rate in stereo, and mixed with live gains in cpal's callback,
//! which also keeps the clock that drives playback.
//!
//! cpal's stream is built and dropped on its own thread, as it isn't `Send`
//! on every system.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::decode::{AudioTrack, discard_others, read_packet, seek_before, start_offset};

/// How much audio the decoding thread keeps ahead of the output.
const AHEAD_SECS: f64 = 0.5;

/// How far before a seek target decoding starts, so the decoder is primed
/// (AAC frames overlap) when the target is reached.
const PREROLL_SECS: f64 = 0.1;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One track's samples waiting to be played.
#[derive(Default)]
struct TrackBuf {
    /// Interleaved stereo, starting at `Buf::start`.
    samples: VecDeque<f32>,
    /// Placed by timestamp once since the last seek.
    synced: bool,
    /// No more samples will come before the next seek.
    done: bool,
}

struct Buf {
    /// Bumped by every seek, so samples decoded before it are dropped.
    generation: u64,
    /// The sample (counted at the output rate from the clip's start) that
    /// the callback plays next, and that each track's queue starts at.
    start: i64,
    tracks: Vec<TrackBuf>,
    gains: Vec<f32>,
    /// The callback plays (and the clock runs) only when active.
    active: bool,
    /// The first sample of the last buffer the callback filled, and when
    /// it's heard.
    anchor: Option<(i64, Instant)>,
    /// A seek for the decoding thread, in seconds.
    seek: Option<f64>,
    quit: bool,
}

impl Buf {
    /// How many samples every track can give: the least any unfinished
    /// track holds, unlimited once they're all finished.
    fn available(&self) -> usize {
        self.tracks
            .iter()
            .filter(|t| !t.done)
            .map(|t| t.samples.len() / 2)
            .min()
            .unwrap_or(usize::MAX)
    }
}

pub(super) struct AudioShared {
    pub rate: u32,
    buf: Mutex<Buf>,
    wake: Condvar,
}

impl AudioShared {
    fn new(rate: u32, tracks: usize) -> AudioShared {
        AudioShared {
            rate,
            buf: Mutex::new(Buf {
                generation: 0,
                start: 0,
                tracks: (0..tracks).map(|_| TrackBuf::default()).collect(),
                gains: Vec::new(),
                active: false,
                anchor: None,
                seek: Some(0.0),
                quit: false,
            }),
            wake: Condvar::new(),
        }
    }

    /// Starts playing from `t` seconds: drops what's buffered and has the
    /// decoding thread seek there.
    pub fn start_at(&self, t: f64) {
        let mut buf = lock(&self.buf);
        buf.generation += 1;
        buf.start = (t * f64::from(self.rate)).round() as i64;
        for track in &mut buf.tracks {
            *track = TrackBuf::default();
        }
        buf.anchor = None;
        buf.seek = Some(t);
        buf.active = true;
        drop(buf);
        self.wake.notify_all();
    }

    /// Stops playing (the callback outputs silence).
    pub fn stop(&self) {
        let mut buf = lock(&self.buf);
        buf.active = false;
        buf.anchor = None;
    }

    pub fn set_gains(&self, gains: &[f32]) {
        lock(&self.buf).gains = gains.to_vec();
    }

    /// The position being heard, in seconds, once the callback has played
    /// something since the last `start_at`.
    pub fn position(&self) -> Option<f64> {
        let buf = lock(&self.buf);
        let (sample, at) = buf.anchor?;
        let rate = f64::from(self.rate);
        let now = Instant::now();
        let t = if now >= at {
            sample as f64 / rate + (now - at).as_secs_f64()
        } else {
            sample as f64 / rate - (at - now).as_secs_f64()
        };
        // Not past what was handed to the device, if callbacks stall.
        Some(t.min(buf.start as f64 / rate))
    }

    /// The callback: fills `out` (interleaved stereo) with the mix.
    /// `latency` is how long until the first sample is heard.
    fn fill(&self, out: &mut [f32], latency: Duration) {
        out.fill(0.0);
        let mut buf = lock(&self.buf);
        if !buf.active {
            return;
        }
        let frames = out.len() / 2;
        let n = frames.min(buf.available());
        if n == 0 {
            // Waiting for the decoder: the clock holds.
            return;
        }
        let Buf { tracks, gains, .. } = &mut *buf;
        for (i, track) in tracks.iter_mut().enumerate() {
            let gain = gains.get(i).copied().unwrap_or(1.0);
            let take = (2 * n).min(track.samples.len());
            if gain != 0.0 {
                for (o, s) in out.iter_mut().zip(track.samples.iter().take(take)) {
                    *o += s * gain;
                }
            }
            track.samples.drain(..take);
        }
        for o in &mut out[..2 * n] {
            *o = o.clamp(-1.0, 1.0);
        }
        let first = buf.start;
        buf.start += n as i64;
        buf.anchor = Some((first, Instant::now() + latency));
    }
}

/// The audio threads; dropping it stops them.
pub(super) struct AudioOut {
    pub shared: Arc<AudioShared>,
    quit_output: Option<mpsc::Sender<()>>,
    threads: Vec<JoinHandle<()>>,
}

impl AudioOut {
    /// Opens the default output and starts decoding the `tracks` audio
    /// tracks of `path`. `None` when the system has no usable output.
    pub fn start(path: &Path, tracks: usize) -> Option<AudioOut> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (quit_tx, quit_rx) = mpsc::channel::<()>();
        let abandoned = Arc::new(AtomicBool::new(false));
        let output = {
            let abandoned = abandoned.clone();
            std::thread::Builder::new()
                .name("montage-audio-out".into())
                .spawn(move || {
                    let stream = match open_output(tracks) {
                        Ok((stream, shared)) => {
                            let _ = ready_tx.send(Some(shared));
                            stream
                        }
                        Err(_) => {
                            let _ = ready_tx.send(None);
                            return;
                        }
                    };
                    if !abandoned.load(Ordering::Relaxed) {
                        // Until the player is dropped.
                        let _ = quit_rx.recv();
                    }
                    drop(stream);
                })
                .ok()?
        };
        let shared = match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Some(shared)) => shared,
            Ok(None) | Err(_) => {
                abandoned.store(true, Ordering::Relaxed);
                drop(quit_tx);
                // A device that hangs isn't waited for.
                return None;
            }
        };
        let decoder = {
            let shared = shared.clone();
            let path = path.to_path_buf();
            std::thread::Builder::new()
                .name("montage-audio-decode".into())
                .spawn(move || decode_thread(&shared, &path))
                .ok()?
        };
        Some(AudioOut {
            shared,
            quit_output: Some(quit_tx),
            threads: vec![output, decoder],
        })
    }
}

impl Drop for AudioOut {
    fn drop(&mut self) {
        lock(&self.shared.buf).quit = true;
        self.shared.wake.notify_all();
        self.quit_output.take();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Builds and starts the output stream, in stereo f32 when the device
/// allows it.
fn open_output(tracks: usize) -> Result<(cpal::Stream, Arc<AudioShared>), String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no output device".to_string())?;
    let default = device.default_output_config().map_err(|e| e.to_string())?;
    let rate = default.sample_rate();
    let mut config = default;
    if default.sample_format() != cpal::SampleFormat::F32
        && let Ok(ranges) = device.supported_output_configs()
    {
        for range in ranges {
            if range.sample_format() == cpal::SampleFormat::F32
                && range.channels() == default.channels()
                && range.min_sample_rate() <= rate
                && rate <= range.max_sample_rate()
            {
                config = range.with_sample_rate(rate);
                break;
            }
        }
    }
    let shared = Arc::new(AudioShared::new(rate, tracks));
    let stream_config = config.config();
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&device, stream_config, shared.clone()),
        cpal::SampleFormat::F64 => build::<f64>(&device, stream_config, shared.clone()),
        cpal::SampleFormat::I16 => build::<i16>(&device, stream_config, shared.clone()),
        cpal::SampleFormat::I32 => build::<i32>(&device, stream_config, shared.clone()),
        cpal::SampleFormat::U16 => build::<u16>(&device, stream_config, shared.clone()),
        cpal::SampleFormat::U8 => build::<u8>(&device, stream_config, shared.clone()),
        cpal::SampleFormat::I8 => build::<i8>(&device, stream_config, shared.clone()),
        other => return Err(format!("unsupported sample format {other:?}")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, shared))
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    shared: Arc<AudioShared>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = usize::from(config.channels).max(1);
    let mut mix: Vec<f32> = Vec::new();
    device.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let frames = data.len() / channels;
            // Grows once to the device's buffer size, then stays.
            mix.resize(frames * 2, 0.0);
            let ts = info.timestamp();
            shared.fill(&mut mix, ts.playback.duration_since(ts.callback));
            for (out, lr) in data.chunks_mut(channels).zip(mix.chunks(2)) {
                if channels == 1 {
                    out[0] = T::from_sample((lr[0] + lr[1]) * 0.5);
                } else {
                    for (c, o) in out.iter_mut().enumerate() {
                        *o = T::from_sample(if c < 2 { lr[c] } else { 0.0 });
                    }
                }
            }
        },
        |_err| {},
        None,
    )
}

/// Decodes every audio track into the shared buffer, staying
/// `AHEAD_SECS` ahead of the output, and seeks when asked.
fn decode_thread(shared: &AudioShared, path: &Path) {
    let Ok(mut ictx) = ffmpeg_next::format::input(path) else {
        return;
    };
    let offset = start_offset(&ictx);
    let Ok(mut tracks) = AudioTrack::open_all(&ictx, shared.rate) else {
        return;
    };
    let streams: Vec<usize> = tracks.iter().map(|t| t.stream).collect();
    discard_others(&mut ictx, &streams);
    let rate = f64::from(shared.rate);
    let ahead = (AHEAD_SECS * rate) as usize;
    let mut generation = u64::MAX;
    let mut eof = false;
    // (track, start in seconds of file time, interleaved stereo)
    let mut pending: Vec<(usize, Option<f64>, Vec<f32>)> = Vec::new();

    loop {
        let seek = {
            let mut buf = lock(&shared.buf);
            loop {
                if buf.quit {
                    return;
                }
                if let Some(t) = buf.seek.take() {
                    generation = buf.generation;
                    break Some(t);
                }
                if !eof && buf.available() < ahead {
                    break None;
                }
                buf = shared
                    .wake
                    .wait_timeout(buf, Duration::from_millis(20))
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        };
        if let Some(t) = seek {
            seek_before(&mut ictx, (t - PREROLL_SECS).max(0.0) + offset);
            for track in &mut tracks {
                track.flush();
            }
            eof = false;
            continue;
        }

        match read_packet(&mut ictx) {
            Some(packet) => {
                let Some(n) = tracks.iter().position(|t| t.stream == packet.stream()) else {
                    continue;
                };
                if tracks[n].decoder.send_packet(&packet).is_err() {
                    continue;
                }
                let _ = tracks[n].drain(&mut |at, _, l, r| pending.push((n, at, interleave(l, r))));
            }
            None => {
                for (n, track) in tracks.iter_mut().enumerate() {
                    let _ = track.decoder.send_eof();
                    let _ = track.drain(&mut |at, _, l, r| pending.push((n, at, interleave(l, r))));
                }
                eof = true;
            }
        }

        let mut buf = lock(&shared.buf);
        if buf.generation != generation {
            pending.clear();
            continue;
        }
        let start = buf.start;
        for (n, at, lr) in pending.drain(..) {
            let track = &mut buf.tracks[n];
            let write = start + (track.samples.len() / 2) as i64;
            let at = at.map(|s| ((s - offset) * rate).round() as i64);
            // Placed by timestamp, then contiguously; resynced when the
            // timestamps jump by more than 100 ms.
            let pos = match at {
                Some(at) if !track.synced || (at - write).abs() > (rate / 10.0) as i64 => at,
                _ => write,
            };
            track.synced = true;
            let mut skip = 0;
            if pos > write {
                let gap = (pos - write) as usize;
                track.samples.extend(std::iter::repeat_n(0.0, gap * 2));
            } else {
                skip = ((write - pos) as usize * 2).min(lr.len());
            }
            track.samples.extend(&lr[skip..]);
        }
        if eof {
            for track in &mut buf.tracks {
                track.done = true;
            }
        }
    }
}

fn interleave(left: &[f32], right: &[f32]) -> Vec<f32> {
    let mut lr = Vec::with_capacity(left.len() * 2);
    for (l, r) in left.iter().zip(right) {
        lr.push(*l);
        lr.push(*r);
    }
    lr
}
