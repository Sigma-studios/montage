//! Playback for the editor's preview: video decoded on the GPU when the
//! system has a decoder (NVDEC or VAAPI on Linux, D3D11VA on Windows,
//! VideoToolbox on macOS; software otherwise), frames copied to the CPU and
//! scaled to the preview's size, audio mixed with live gains and played
//! through cpal. The audio clock drives playback; video frames follow it.
//!
//! Threads: one decodes the video (a few frames ahead), one decodes the
//! audio (half a second ahead), and cpal's callback mixes it. The methods
//! of [`Player`] only take a lock and return.
//!
//! **The clock.** At 1x with audio, the position is the sample being heard:
//! the callback notes which sample starts each buffer and when it will be
//! heard (cpal's output latency), and the position runs on from there,
//! never past what was handed to the device. Without audio tracks, without
//! an output device (or with `MONTAGE_NO_AUDIO=1`), or at another speed, a
//! wall clock runs instead. After a seek, the clock holds until the frame
//! at the target is decoded, so audio and video start together.
//!
//! **Shuttle.** Speeds other than 1x play without sound. Negative speeds
//! play backwards by showing the frame at the (backwards running) clock
//! whenever the decoder catches up: the frames just before the last exact
//! seek are kept, then the decoder seeks again; on long GOPs it's choppy.
//!
//! **Memory.** At most 12 frames (and 64 MB) decoded ahead, and as many
//! kept behind the shown frame for stepping back while paused.
//!
//! `MONTAGE_HWDEC=0` forces software decoding.

mod audio;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ffmpeg_next::frame;
use ffmpeg_next::software::scaling;
use montage_core::render::RgbaImage;

use self::audio::{AudioOut, AudioShared};
use crate::decode::{Decoded, Threads, ToRgba, VideoSource, download, fit};
use crate::{MediaError, MediaInfo};

/// Frames decoded ahead of the shown one, at most.
const AHEAD_FRAMES: usize = 12;
/// Frames kept behind the shown one for stepping back, at most.
const BEHIND_FRAMES: usize = 12;
/// Bytes of frames kept ahead, and behind.
const BUDGET_BYTES: usize = 64 << 20;
/// Slack when comparing timestamps, in seconds.
const EPS: f64 = 1e-4;
/// A seek at most this far past the decoded frames decodes on to it
/// rather than seeking back to a keyframe (scrubbing forwards).
const DECODE_ON_SECS: f64 = 1.0;

/// A decoded frame, scaled to the preview size.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Seconds from the start of the original.
    pub pts: f64,
    pub image: RgbaImage,
}

impl VideoFrame {
    fn bytes(&self) -> usize {
        self.image.data.len()
    }
}

/// Plays one clip. Every method returns at once: decoding and audio run on
/// their own threads.
pub struct Player {
    info: MediaInfo,
    shared: Arc<Shared>,
    video: Option<JoinHandle<()>>,
    // Dropped after the video thread is stopped.
    audio: Option<AudioOut>,
}

struct Shared {
    state: Mutex<State>,
    /// Wakes the video thread.
    wake: Condvar,
    /// `State::generation`, readable without the lock, so a long seek can
    /// be abandoned when another one comes.
    generation: AtomicU64,
    audio: Option<Arc<AudioShared>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Clone, Copy)]
struct Await {
    t: f64,
    /// The shown frame is the one at `t` if no frame comes before it.
    shown_ok: bool,
}

struct State {
    duration: f64,
    frame_dur: f64,
    playing: bool,
    speed: f64,
    loop_range: Option<(f64, f64)>,
    preview: (u32, u32),
    source_size: (u32, u32),

    /// The position when the clock is stopped, or when it last started.
    base: f64,
    /// When the wall clock started, if it's running.
    since: Option<Instant>,
    /// The last position reported while playing forwards, so the audio
    /// clock's jitter never moves it back.
    last: f64,
    /// Waiting for the decoder to deliver the frame at a seek target: the
    /// clock holds meanwhile.
    waiting: bool,

    /// Bumped whenever the decoder must start over; frames from an older
    /// generation are dropped.
    generation: u64,
    /// A seek for the video thread.
    request: Option<f64>,
    /// The video thread is decoding up to a seek target.
    in_flight: bool,
    /// The video thread is scaling the frames before a seek target, which
    /// is shown already.
    filling: bool,
    /// A seek waiting for `filling` to end, as the frames behind may
    /// serve it: (target, gate, snap).
    deferred: Option<(f64, bool, bool)>,
    /// A seek served by decoding on: the frames up to it are dropped
    /// until the one after it comes.
    awaiting: Option<Await>,
    /// The seek in flight is a step: the position snaps to its frame.
    snap: bool,
    /// The decoder reached the end of the clip.
    eof: bool,
    /// Frames on their way from the decoder to `ahead`.
    converting: usize,
    /// A hardware frame couldn't be read: decode in software.
    hw_failed: bool,

    /// The frames following the shown one, in order.
    ahead: VecDeque<VideoFrame>,
    ahead_bytes: usize,
    /// A frame was skipped between the shown one and `ahead`.
    gap: bool,
    /// The frames before the shown one, ending with it, when known.
    behind: VecDeque<VideoFrame>,
    behind_bytes: usize,
    /// A frame to show whatever the clock says: the result of a seek.
    forced: Option<VideoFrame>,
    /// The pts of the frame shown (or about to be).
    shown_pts: f64,
    /// The pts and size of the last frame `take_frame` returned.
    returned: Option<(f64, u32, u32)>,

    decoder_name: String,
    quit: bool,
}

impl State {
    fn running(&self) -> bool {
        self.playing && !self.waiting
    }

    fn audio_clock(&self, audio: Option<&AudioShared>) -> bool {
        audio.is_some() && self.speed == 1.0
    }

    fn position(&mut self, audio: Option<&AudioShared>) -> f64 {
        if !self.running() {
            return self.base;
        }
        let p = match audio {
            Some(a) if self.audio_clock(audio) => {
                a.position().map_or(self.base, |p| p.max(self.base))
            }
            _ => {
                let elapsed = self.since.map_or(0.0, |s| s.elapsed().as_secs_f64());
                self.base + elapsed * self.speed
            }
        };
        let p = if self.speed > 0.0 {
            p.max(self.last)
        } else {
            p
        };
        self.last = p;
        p.clamp(0.0, self.duration)
    }

    /// Stops the clock where it is.
    fn freeze(&mut self, audio: Option<&AudioShared>) {
        let p = self.position(audio);
        self.base = p;
        self.last = p;
        self.since = None;
        if let Some(a) = audio {
            a.stop();
        }
    }

    /// Starts the clock from `base` if playback should run.
    fn thaw(&mut self, audio: Option<&AudioShared>) {
        self.last = self.base;
        if !self.running() {
            return;
        }
        match audio {
            Some(a) if self.audio_clock(audio) => a.start_at(self.base),
            _ => self.since = Some(Instant::now()),
        }
    }

    fn end(&self) -> f64 {
        self.loop_range.map_or(self.duration, |(_, b)| b)
    }

    fn start(&self) -> f64 {
        self.loop_range.map_or(0.0, |(a, _)| a)
    }

    /// Loops or stops at the ends.
    fn advance(&mut self, audio: Option<&AudioShared>) {
        if !self.running() {
            return;
        }
        let p = self.position(audio);
        if self.speed > 0.0 && p >= self.end() {
            match self.loop_range {
                Some((a, _)) => self.seek_to(a, audio, true, false),
                None => {
                    self.freeze(audio);
                    self.playing = false;
                    self.base = self.duration;
                    self.last = self.duration;
                }
            }
        } else if self.speed < 0.0 && p <= self.start() {
            match self.loop_range {
                Some((_, b)) => self.seek_to(b - self.frame_dur, audio, false, false),
                None => {
                    self.freeze(audio);
                    self.playing = false;
                    self.base = 0.0;
                    self.last = 0.0;
                }
            }
        }
    }

    /// Moves to `t`: from the frames at hand when they cover it, else
    /// through an exact seek of the decoder. With `gate`, the clock holds
    /// until the frame is decoded.
    fn seek_to(&mut self, t: f64, audio: Option<&AudioShared>, gate: bool, snap: bool) {
        let t = t.clamp(0.0, self.duration);
        self.freeze(audio);
        self.base = t;
        self.waiting = false;
        if self.filling && !self.in_flight && self.request.is_none() {
            self.deferred = Some((t, gate, snap));
            self.waiting = gate;
            self.thaw(audio);
            return;
        }
        let was_awaiting = self.awaiting.take().is_some();
        let busy = self.in_flight || self.request.is_some() || self.deferred.is_some();
        let served = if busy {
            false
        } else if t + EPS >= self.shown_pts {
            self.serve_ahead(t)
        } else {
            !was_awaiting && self.serve_behind(t)
        };
        if served {
            if snap {
                self.base = self.shown_pts;
            }
        } else if !busy && self.decode_on(t) {
            self.snap = snap;
            self.waiting = gate;
            self.resolve_awaiting(audio);
        } else {
            self.bump();
            self.request = Some(t);
            self.snap = snap;
            self.waiting = gate;
        }
        self.thaw(audio);
    }

    /// A seek is on its way.
    fn pending(&self) -> bool {
        self.in_flight
            || self.request.is_some()
            || self.deferred.is_some()
            || self.awaiting.is_some()
    }

    /// Starts serving a seek to `t` by decoding on from the frames ahead,
    /// when it's close enough past them.
    fn decode_on(&mut self, t: f64) -> bool {
        let tail = self.ahead.back().map_or(self.shown_pts, |f| f.pts);
        let usable = self.speed > 0.0
            && !self.at_end()
            && t + EPS >= self.shown_pts
            && t - tail <= DECODE_ON_SECS
            && self.ahead.front().is_none_or(|f| f.pts <= t + EPS);
        if !usable {
            return false;
        }
        // The shown frame is the one at `t` if nothing comes before it.
        let shown_ok = self.ahead.is_empty() && !self.gap;
        while self.ahead.len() > 1 {
            self.pop_ahead();
        }
        self.clear_behind();
        self.gap = true;
        self.awaiting = Some(Await { t, shown_ok });
        true
    }

    /// Shows the frame at the awaited target once the one after it (or
    /// the end) is decoded, dropping the ones before.
    fn resolve_awaiting(&mut self, audio: Option<&AudioShared>) {
        let Some(aw) = self.awaiting else {
            return;
        };
        while self.ahead.len() >= 2 && self.ahead[1].pts <= aw.t + EPS {
            self.pop_ahead();
        }
        let next_known = self.ahead.back().is_some_and(|f| f.pts > aw.t + EPS);
        if !next_known && !self.at_end() {
            return;
        }
        self.awaiting = None;
        if self.ahead.front().is_some_and(|f| f.pts <= aw.t + EPS) {
            let f = self.pop_ahead().expect("checked above");
            self.gap = false;
            self.show(f);
        } else if aw.shown_ok {
            self.gap = false;
        } else {
            // The frame at the target went by (variable frame rate).
            self.bump();
            self.request = Some(aw.t);
            return;
        }
        if self.snap && !self.playing {
            self.base = self.shown_pts;
            self.last = self.shown_pts;
        }
        if self.waiting {
            self.waiting = false;
            self.thaw(audio);
        }
    }

    /// Drops every decoded frame and has the decoder start over.
    fn bump(&mut self) {
        self.generation += 1;
        self.filling = false;
        self.deferred = None;
        self.awaiting = None;
        self.ahead.clear();
        self.ahead_bytes = 0;
        self.gap = false;
        self.forced = None;
        self.eof = false;
    }

    /// Shows the frame at `t` from the frames decoded ahead, if they
    /// cover it.
    fn serve_ahead(&mut self, t: f64) -> bool {
        let Some(first) = self.ahead.front() else {
            // Nothing decoded after the shown frame yet: it covers `t` only
            // at the end of the clip.
            return self.at_end() && !self.gap;
        };
        if t + EPS < first.pts {
            // Between the shown frame and the next one.
            return !self.gap;
        }
        let Some(last_due) = self.ahead.iter().rposition(|f| f.pts <= t + EPS) else {
            return false;
        };
        if last_due + 1 == self.ahead.len() && !self.at_end() {
            // The next frame isn't decoded yet: it might be due too.
            return false;
        }
        for _ in 0..last_due {
            let f = self.pop_ahead().expect("counted above");
            self.push_behind(f);
        }
        let f = self.pop_ahead().expect("counted above");
        self.gap = false;
        self.show(f);
        true
    }

    /// Shows the frame at `t` from the frames kept behind the shown one,
    /// moving the ones after it back ahead.
    fn serve_behind(&mut self, t: f64) -> bool {
        if !self.behind_ok() || self.behind.front().is_none_or(|f| f.pts > t + EPS) {
            return false;
        }
        let Some(i) = self.behind.iter().rposition(|f| f.pts <= t + EPS) else {
            return false;
        };
        self.forced = None;
        while self.behind.len() > i + 1 {
            let f = self.behind.pop_back().expect("counted above");
            self.behind_bytes -= f.bytes();
            self.ahead_bytes += f.bytes();
            self.ahead.push_front(f);
        }
        let f = self.behind.back().expect("kept above").clone();
        self.shown_pts = f.pts;
        self.forced = Some(f);
        true
    }

    /// Whether `behind` ends with the shown frame (else it's stale).
    fn behind_ok(&self) -> bool {
        self.behind
            .back()
            .is_some_and(|f| (f.pts - self.shown_pts).abs() < EPS)
    }

    fn pop_ahead(&mut self) -> Option<VideoFrame> {
        let f = self.ahead.pop_front()?;
        self.ahead_bytes -= f.bytes();
        Some(f)
    }

    fn push_behind(&mut self, f: VideoFrame) {
        self.behind_bytes += f.bytes();
        self.behind.push_back(f);
        while self.behind.len() > BEHIND_FRAMES
            || (self.behind_bytes > BUDGET_BYTES && self.behind.len() > 1)
        {
            let old = self.behind.pop_front().expect("len > 1");
            self.behind_bytes -= old.bytes();
        }
    }

    fn push_behind_if_newer(&mut self, f: VideoFrame) {
        if self.behind.back().is_none_or(|b| b.pts < f.pts - EPS) {
            self.push_behind(f);
        }
    }

    fn clear_behind(&mut self) {
        self.behind.clear();
        self.behind_bytes = 0;
    }

    /// Makes `f` the frame to show next, kept behind for stepping back.
    fn show(&mut self, f: VideoFrame) {
        self.shown_pts = f.pts;
        self.push_behind(f.clone());
        self.forced = Some(f);
    }

    fn wants_more(&self) -> bool {
        if self.awaiting.is_some() {
            // The frames ahead are dropped as they come.
            return !self.eof && self.request.is_none();
        }
        let frames = self.ahead.len() + self.converting;
        let frame_bytes = self.preview.0 as usize * self.preview.1 as usize * 4;
        let bytes = self.ahead_bytes + self.converting * frame_bytes;
        !self.eof
            && self.request.is_none()
            && self.speed > 0.0
            && (frames < 2 || (frames < AHEAD_FRAMES && bytes < BUDGET_BYTES))
    }

    /// No frame will come after the ones ahead.
    fn at_end(&self) -> bool {
        self.eof && self.converting == 0
    }

    /// Whether a frame decoded for playing forwards comes too late: the
    /// frame after it is due already.
    fn late(&mut self, pts: f64, audio: Option<&AudioShared>) -> bool {
        self.running()
            && self.speed > 0.0
            && self.ahead.is_empty()
            && self.forced.is_none()
            && pts + self.frame_dur < self.position(audio)
    }

    /// Playing backwards: asks for the frame at the clock when it moved
    /// past the shown one.
    fn reverse_tick(&mut self, audio: Option<&AudioShared>) {
        if !self.running() || self.speed >= 0.0 || self.pending() || self.filling {
            return;
        }
        let p = self.position(audio);
        if p + EPS < self.shown_pts && !self.serve_behind(p) {
            self.bump();
            self.request = Some(p);
            self.snap = false;
        }
    }
}

impl Player {
    /// Opens `path`, paused at 0 s, decoding frames to fit within
    /// `max_width` x `max_height` (aspect ratio kept).
    ///
    /// Frames are never scaled up: a preview larger than the clip gets
    /// frames at the clip's size.
    pub fn open(path: &Path, max_width: u32, max_height: u32) -> Result<Player, MediaError> {
        let info = crate::probe(path)?;
        let source = VideoSource::open(path, true, Threads::Auto)?;
        let no_audio = std::env::var("MONTAGE_NO_AUDIO").is_ok_and(|v| !v.is_empty() && v != "0");
        let audio = if info.audio_tracks > 0 && !no_audio {
            AudioOut::start(path, info.audio_tracks)
        } else {
            None
        };
        let fps = source.fps;
        let state = State {
            duration: info.duration.max(0.0),
            frame_dur: 1.0 / fps,
            playing: false,
            speed: 1.0,
            loop_range: None,
            preview: fit(source.width, source.height, max_width, max_height),
            source_size: (source.width, source.height),
            base: 0.0,
            since: None,
            last: 0.0,
            waiting: true,
            generation: 1,
            request: Some(0.0),
            in_flight: false,
            filling: false,
            deferred: None,
            awaiting: None,
            snap: false,
            eof: false,
            converting: 0,
            hw_failed: false,
            ahead: VecDeque::new(),
            ahead_bytes: 0,
            gap: false,
            behind: VecDeque::new(),
            behind_bytes: 0,
            forced: None,
            shown_pts: 0.0,
            returned: None,
            decoder_name: source.decoder_name(),
            quit: false,
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            wake: Condvar::new(),
            generation: AtomicU64::new(1),
            audio: audio.as_ref().map(|a| a.shared.clone()),
        });
        let video = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("montage-video".into())
                .spawn(move || video_thread(&shared, source))
                .map_err(|source| MediaError::Io {
                    action: "can't start the video thread for",
                    path: path.into(),
                    source,
                })?
        };
        Ok(Player {
            info,
            shared,
            video: Some(video),
            audio,
        })
    }

    pub fn info(&self) -> &MediaInfo {
        &self.info
    }

    /// The decoder in use, for the status line: `hevc (NVDEC)`,
    /// `h264 (software)`…
    pub fn decoder_name(&self) -> String {
        self.shared.lock().decoder_name.clone()
    }

    /// Whether the audio plays (and drives the clock): false for a clip
    /// without audio, or when the system has no output device.
    pub fn has_audio_output(&self) -> bool {
        self.audio.is_some()
    }

    /// Runs `f` on the state, then wakes the video thread.
    fn with<R>(&self, f: impl FnOnce(&mut State, Option<&AudioShared>) -> R) -> R {
        let audio = self.shared.audio.as_deref();
        let mut st = self.shared.lock();
        let r = f(&mut st, audio);
        self.shared
            .generation
            .store(st.generation, Ordering::Release);
        drop(st);
        self.shared.wake.notify_all();
        r
    }

    pub fn play(&mut self) {
        self.with(|st, audio| {
            if st.playing {
                return;
            }
            let p = st.position(audio);
            let half = st.frame_dur / 2.0;
            if st.speed > 0.0 && (p >= st.end() - half || p + EPS < st.start()) {
                st.seek_to(st.start(), audio, true, false);
            } else if st.speed < 0.0 && (p <= st.start() + half || p > st.end()) {
                st.seek_to(st.end() - st.frame_dur, audio, true, false);
            }
            st.freeze(audio);
            st.playing = true;
            st.thaw(audio);
        });
    }

    pub fn pause(&mut self) {
        self.with(|st, audio| {
            st.freeze(audio);
            st.playing = false;
        });
    }

    pub fn is_playing(&self) -> bool {
        self.shared.lock().playing
    }

    /// Playback speed for J/K/L shuttle: 1.0 normal, 2.0, 4.0…; negative
    /// plays backwards (at least by stepping back frame by frame).
    ///
    /// Only 1x plays sound. 0 pauses.
    pub fn set_speed(&mut self, speed: f64) {
        if !speed.is_finite() {
            return;
        }
        self.with(|st, audio| {
            st.freeze(audio);
            if speed == 0.0 {
                st.playing = false;
            } else {
                st.speed = speed;
            }
            st.thaw(audio);
        });
    }

    /// Seeks to `t` seconds, exactly (not to the keyframe before).
    pub fn seek(&mut self, t: f64) {
        if !t.is_finite() {
            return;
        }
        self.with(|st, audio| st.seek_to(t, audio, true, false));
    }

    /// Steps `frames` frames forwards (or backwards when negative), paused.
    pub fn step(&mut self, frames: i32) {
        self.with(|st, audio| {
            st.freeze(audio);
            st.playing = false;
            let from = if st.pending() { st.base } else { st.shown_pts };
            let target = (from + f64::from(frames) * st.frame_dur).clamp(0.0, st.duration);
            // Aim at the middle of the frame, away from rounding.
            let aim = (target + st.frame_dur / 2.0).min(st.duration);
            st.seek_to(aim, audio, true, true);
            if st.pending() {
                st.base = target;
                st.last = target;
            }
        });
    }

    /// The current position, in seconds.
    pub fn position(&self) -> f64 {
        self.with(|st, audio| {
            st.advance(audio);
            st.position(audio)
        })
    }

    /// Linear gain of each audio track (missing entries are 1.0; 0.0 mutes),
    /// applied live.
    pub fn set_gains(&mut self, gains: &[f32]) {
        if let Some(audio) = &self.shared.audio {
            audio.set_gains(gains);
        }
    }

    /// Replays `start..end` in a loop while playing, or plays to the end of
    /// the clip when `None`. Playback stops at `end` without a loop.
    pub fn set_loop(&mut self, range: Option<(f64, f64)>) {
        self.with(|st, audio| {
            st.loop_range = range.and_then(|(a, b)| {
                let (a, b) = (a.min(b).max(0.0), a.max(b).min(st.duration));
                (a.is_finite() && b.is_finite() && b - a > EPS).then_some((a, b))
            });
            if let Some((a, b)) = st.loop_range
                && st.running()
            {
                let p = st.position(audio);
                if p + EPS < a || p >= b {
                    let to = if st.speed > 0.0 { a } else { b - st.frame_dur };
                    st.seek_to(to, audio, true, false);
                }
            }
        });
    }

    /// Changes the size frames are scaled to fit within (the window was
    /// resized).
    pub fn set_preview_size(&mut self, max_width: u32, max_height: u32) {
        self.with(|st, _| {
            let (w, h) = st.source_size;
            let size = fit(w, h, max_width, max_height);
            if size == st.preview {
                return;
            }
            st.preview = size;
            st.clear_behind();
            if !st.playing {
                // Decode the shown frame again at the new size.
                st.bump();
                st.request = Some(st.shown_pts + EPS);
                st.snap = false;
                st.waiting = true;
            }
        });
    }

    /// The frame to show now, if it changed since the last call.
    pub fn take_frame(&mut self) -> Option<VideoFrame> {
        self.with(|st, audio| {
            st.advance(audio);
            if st.deferred.is_some() || st.awaiting.is_some() {
                // What's at hand is from before the seek.
                return None;
            }
            let p = st.position(audio);
            let forwards = st.running() && st.speed > 0.0;
            // What's behind still leads to what's ahead.
            let contiguous = st.behind_ok() && !st.gap;
            let mut newest = st.forced.take();
            let mut passed = Vec::new();
            while st.ahead.front().is_some_and(|f| f.pts <= p + EPS) {
                let f = st.pop_ahead().expect("checked above");
                if let Some(old) = newest.replace(f) {
                    passed.push(old);
                }
            }
            let f = newest?;
            st.gap = false;
            if forwards {
                // Not kept while playing: stepping back after a pause
                // seeks.
                st.clear_behind();
            } else {
                if !contiguous {
                    st.clear_behind();
                }
                for old in passed {
                    st.push_behind_if_newer(old);
                }
                if st.behind.back().is_none_or(|b| b.pts < f.pts - EPS) {
                    st.push_behind(f.clone());
                }
            }
            st.shown_pts = f.pts;
            // A seek back to the frame on screen changes nothing.
            let id = (f.pts, f.image.width, f.image.height);
            if st.returned == Some(id) {
                return None;
            }
            st.returned = Some(id);
            Some(f)
        })
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.lock().quit = true;
        self.shared.wake.notify_all();
        if let Some(t) = self.video.take() {
            let _ = t.join();
        }
        self.audio.take();
    }
}

enum Job {
    Seek {
        target: f64,
        generation: u64,
        size: (u32, u32),
        keep_behind: bool,
        frame_dur: f64,
    },
    Next {
        generation: u64,
        size: (u32, u32),
    },
    Software,
}

/// A decoded frame on its way to `ahead`.
struct Convert {
    generation: u64,
    pts: f64,
    frame: frame::Video,
    size: (u32, u32),
    map: bool,
}

/// Decodes; the frames for playing forwards are copied from the GPU and
/// scaled on a second thread, so both run at once.
fn video_thread(shared: &Shared, mut source: VideoSource) {
    let audio = shared.audio.as_deref();
    let mut rgba = ToRgba::new(scaling::Flags::BILINEAR);
    let mut hw = source.hw();
    let (to_convert, converts) = mpsc::sync_channel::<Convert>(2);
    std::thread::scope(|s| {
        // Moved in, so returning stops the converter.
        let to_convert = to_convert;
        let converter = std::thread::Builder::new()
            .name("montage-video-convert".into())
            .spawn_scoped(s, move || convert_thread(shared, &converts));
        if converter.is_err() {
            return;
        }
        loop {
            let job = {
                let mut st = shared.lock();
                loop {
                    if st.quit {
                        return;
                    }
                    st.advance(audio);
                    st.reverse_tick(audio);
                    shared.generation.store(st.generation, Ordering::Release);
                    if st.hw_failed {
                        st.hw_failed = false;
                        break Job::Software;
                    }
                    if let Some(target) = st.request.take() {
                        st.in_flight = true;
                        break Job::Seek {
                            target,
                            generation: st.generation,
                            size: st.preview,
                            keep_behind: !st.playing || st.speed < 0.0,
                            frame_dur: st.frame_dur,
                        };
                    }
                    if st.wants_more() {
                        break Job::Next {
                            generation: st.generation,
                            size: st.preview,
                        };
                    }
                    let timeout = if st.playing { 4 } else { 50 };
                    st = shared
                        .wake
                        .wait_timeout(st, Duration::from_millis(timeout))
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            };
            match job {
                Job::Seek {
                    target,
                    generation,
                    size,
                    keep_behind,
                    frame_dur,
                } => exact_seek(
                    shared,
                    &mut source,
                    &mut rgba,
                    target,
                    generation,
                    size,
                    keep_behind,
                    frame_dur,
                ),
                Job::Next { generation, size } => {
                    next_frame(shared, &mut source, &to_convert, generation, size)
                }
                Job::Software => {
                    let _ = source.force_software();
                    let mut st = shared.lock();
                    let shown = st.shown_pts;
                    st.freeze(audio);
                    st.bump();
                    st.request = Some(shown + EPS);
                    st.snap = false;
                    st.waiting = st.playing;
                }
            }
            if source.hw() != hw {
                hw = source.hw();
                shared.lock().decoder_name = source.decoder_name();
            }
        }
    });
}

/// Decodes the next frame and hands it to the converter.
fn next_frame(
    shared: &Shared,
    source: &mut VideoSource,
    to_convert: &mpsc::SyncSender<Convert>,
    generation: u64,
    size: (u32, u32),
) {
    let decoded = source.next_frame(|_| true, false);
    let mut st = shared.lock();
    if st.generation != generation {
        return;
    }
    match decoded {
        Ok(Some(Decoded {
            pts,
            frame: Some(frame),
        })) => {
            st.converting += 1;
            drop(st);
            let job = Convert {
                generation,
                pts,
                frame,
                size,
                map: source.maps_frames(),
            };
            if to_convert.send(job).is_err() {
                shared.lock().converting -= 1;
            }
        }
        Ok(Some(Decoded { frame: None, .. })) => {}
        Ok(None) | Err(_) => {
            st.eof = true;
            st.resolve_awaiting(shared.audio.as_deref());
        }
    }
}

/// Copies decoded frames from the GPU and scales them into `ahead`, in
/// order. Frames already late aren't.
fn convert_thread(shared: &Shared, converts: &mpsc::Receiver<Convert>) {
    let audio = shared.audio.as_deref();
    let mut rgba = ToRgba::new(scaling::Flags::BILINEAR);
    while let Ok(job) = converts.recv() {
        {
            let mut st = shared.lock();
            let current = st.generation == job.generation;
            let superseded = st
                .awaiting
                .is_some_and(|aw| job.pts + 1.5 * st.frame_dur <= aw.t);
            if !current || superseded || st.late(job.pts, audio) {
                st.converting -= 1;
                if current {
                    // What's behind no longer leads to what's ahead.
                    st.gap = true;
                    st.clear_behind();
                    st.resolve_awaiting(audio);
                }
                drop(st);
                shared.wake.notify_all();
                continue;
            }
        }
        let (w, h) = job.size;
        let image = download(job.frame, job.map).map(|f| rgba.convert(&f, w, h));
        let mut st = shared.lock();
        st.converting -= 1;
        if st.generation == job.generation {
            match image {
                Ok(Ok(image)) => {
                    let f = VideoFrame {
                        pts: job.pts,
                        image,
                    };
                    st.ahead_bytes += f.bytes();
                    st.ahead.push_back(f);
                }
                // A frame the GPU wouldn't give back.
                Err(_) => st.hw_failed = true,
                Ok(Err(_)) => st.eof = true,
            }
            st.resolve_awaiting(audio);
        }
        drop(st);
        shared.wake.notify_all();
    }
}

/// Seeks to the keyframe before `target`, decodes up to it and shows the
/// frame at `target`. The frames just before it are kept for stepping back
/// (`keep_behind`), the one after it goes ahead.
#[allow(clippy::too_many_arguments)]
fn exact_seek(
    shared: &Shared,
    source: &mut VideoSource,
    rgba: &mut ToRgba,
    target: f64,
    generation: u64,
    size: (u32, u32),
    keep_behind: bool,
    frame_dur: f64,
) {
    let audio = shared.audio.as_deref();
    let frame_bytes = (size.0 as usize * size.1 as usize * 4).max(1);
    let keep = if keep_behind {
        (BEHIND_FRAMES - 1).min(BUDGET_BYTES / frame_bytes)
    } else {
        0
    };
    let window = target - (keep as f64 + 1.5) * frame_dur;
    let abandoned = || shared.generation.load(Ordering::Acquire) != generation;

    source.seek(target);
    let mut before: VecDeque<(f64, frame::Video)> = VecDeque::new();
    let mut after = None;
    loop {
        if abandoned() {
            return;
        }
        match source.next_frame(|pts| pts >= window, true) {
            Ok(Some(Decoded {
                pts,
                frame: Some(frame),
            })) => {
                if pts <= target + EPS {
                    before.push_back((pts, frame));
                    if before.len() > keep + 1 {
                        before.pop_front();
                    }
                } else {
                    after = Some((pts, frame));
                    break;
                }
            }
            Ok(Some(Decoded { frame: None, .. })) => {}
            Ok(None) | Err(_) => break,
        }
    }
    // Nothing to show: release the clock.
    let give_up = || {
        let mut st = shared.lock();
        if st.generation == generation {
            st.in_flight = false;
            st.eof = true;
            if st.waiting {
                st.waiting = false;
                st.thaw(audio);
            }
        }
    };
    // The target is before the first frame: show the first.
    let Some(shown) = before.pop_back().or_else(|| after.take()) else {
        return give_up();
    };
    let Ok(image) = rgba.convert(&shown.1, size.0, size.1) else {
        return give_up();
    };
    let shown = VideoFrame {
        pts: shown.0,
        image,
    };
    // The next frame goes ahead with it, so a step forwards finds it.
    let after = after.and_then(|(pts, frame)| {
        rgba.convert(&frame, size.0, size.1)
            .ok()
            .map(|image| VideoFrame { pts, image })
    });
    {
        let mut st = shared.lock();
        if st.generation != generation {
            return;
        }
        st.in_flight = false;
        st.clear_behind();
        st.show(shown.clone());
        if st.snap && !st.playing {
            st.base = shown.pts;
            st.last = shown.pts;
        }
        match after {
            Some(f) => {
                st.ahead_bytes += f.bytes();
                st.ahead.push_back(f);
            }
            None => st.eof = true,
        }
        st.filling = !before.is_empty();
        if st.waiting {
            st.waiting = false;
            st.thaw(audio);
        }
    }
    shared.wake.notify_all();

    // The frames before, for stepping back.
    let mut scaled = Vec::with_capacity(before.len());
    for (pts, frame) in &before {
        if abandoned() {
            return;
        }
        if let Ok(image) = rgba.convert(frame, size.0, size.1) {
            scaled.push(VideoFrame { pts: *pts, image });
        }
    }
    let mut st = shared.lock();
    if st.generation != generation {
        return;
    }
    if st.behind.len() == 1 && st.behind_ok() && st.shown_pts == shown.pts {
        let last = st.behind.pop_back().expect("checked above");
        st.behind_bytes -= last.bytes();
        for f in scaled {
            st.push_behind(f);
        }
        st.push_behind(last);
    }
    st.filling = false;
    if let Some((t, gate, snap)) = st.deferred.take() {
        st.seek_to(t, audio, gate, snap);
    }
}
