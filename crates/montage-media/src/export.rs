//! The pipeline every re-encoding export shares, and the Share export on
//! top of it: H.264 (libx264, two passes so the size lands on the budget)
//! and AAC, in an MP4 with the index at the front.
//!
//! The audio is decoded and mixed down first, into memory (a 75 s stereo
//! mix is about 30 MB). The video is then decoded, scaled to the export's
//! size and frame rate, captioned when there's an overlay, and handed to the
//! encoder: twice for Share (x264's first pass, then the second pass, which
//! is muxed with the audio), once for Full quality and GIF.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec::{self, Capabilities, threading};
use ffmpeg_next::format::{Pixel, Sample, sample};
use ffmpeg_next::software::{resampling, scaling};
use ffmpeg_next::{ChannelLayout, Dictionary, Packet, Rational, encoder, frame, media};
use montage_core::edit::{Cut, EDGE_FADE_SECS};
use montage_core::render::{Overlay, RgbaImage};
use montage_core::share::SharePlan;

use crate::composite;
use crate::encoders::{self, Pass, VideoEncoder};
use crate::{Context, MediaError, init};

/// Sample rate of the exported audio.
pub(crate) const RATE: i32 = 48_000;

const PLANAR_F32: Sample = Sample::F32(sample::Type::Planar);

/// Encodes `plan.range` of `input` to `output` as H.264 (libx264, two-pass
/// at `plan.video_kbps`) + AAC (`plan.audio_kbps`), MP4 with faststart.
///
/// `gains[i]` is the linear gain of audio track `i` (missing entries are
/// 1.0; 0.0 mutes). All tracks are mixed down to one stereo 48 kHz track,
/// with a fade of [`EDGE_FADE_SECS`] at both ends; a clip without audio
/// exports without an audio track. The video is scaled to `plan.width` x
/// `plan.height` and converted to `plan.fps`.
///
/// Writes to a temporary file next to `output` and renames it at the end,
/// so a failed export leaves nothing. `progress` gets 0.0..=1.0 across both
/// passes.
pub fn export_share(
    input: &Path,
    output: &Path,
    plan: &SharePlan,
    gains: &[f32],
    progress: &mut dyn FnMut(f32),
) -> Result<(), MediaError> {
    share_once(
        input,
        output,
        plan,
        gains,
        None,
        &AtomicBool::new(false),
        progress,
    )
}

/// [`export_share`], then checks the size: x264's two-pass can overshoot
/// its target on content too complex for the bitrate, so an export over
/// `max_bytes` is redone at a bitrate lowered by the overshoot, up to twice
/// more. `progress` restarts from 0.0 on each retry.
pub fn export_share_within(
    input: &Path,
    output: &Path,
    plan: &SharePlan,
    gains: &[f32],
    max_bytes: u64,
    progress: &mut dyn FnMut(f32),
) -> Result<(), MediaError> {
    share_within(
        input,
        output,
        plan,
        gains,
        max_bytes,
        None,
        &AtomicBool::new(false),
        progress,
    )
    .map(|_| ())
}

/// [`export_share_within`] with captions and cancelling. Returns the size
/// and the video bitrate of the attempt that fit.
#[allow(clippy::too_many_arguments)]
pub(crate) fn share_within(
    input: &Path,
    output: &Path,
    plan: &SharePlan,
    gains: &[f32],
    max_bytes: u64,
    mut overlay: Option<&mut (dyn Overlay + '_)>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<(u64, u32), MediaError> {
    let mut plan = *plan;
    for attempt in 0..3 {
        share_once(
            input,
            output,
            &plan,
            gains,
            overlay.as_deref_mut(),
            cancel,
            progress,
        )?;
        let size = file_size(output)?;
        if size <= max_bytes || attempt == 2 {
            if size > max_bytes {
                let _ = fs::remove_file(output);
                return Err(MediaError::OverBudget { size, max_bytes });
            }
            return Ok((size, plan.video_kbps));
        }
        // Take the whole overshoot off the video, plus 5 %.
        let seconds = plan.range.duration();
        let over_kbps = (size - max_bytes) as f64 * 8.0 / 1000.0 / seconds;
        let video = (f64::from(plan.video_kbps) - over_kbps) * 0.95;
        plan.video_kbps = video.max(50.0) as u32;
    }
    unreachable!("the last attempt returns")
}

/// One Share export at the plan's bitrate.
fn share_once(
    input: &Path,
    output: &Path,
    plan: &SharePlan,
    gains: &[f32],
    mut overlay: Option<&mut (dyn Overlay + '_)>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<(), MediaError> {
    init();
    if plan.range.duration() <= 0.0 {
        return Err(MediaError::EmptyRange);
    }
    let fps = Rational::from(if plan.fps > 0.0 { plan.fps } else { 30.0 }).reduce();
    let spec = VideoSpec {
        range: plan.range,
        width: plan.width,
        height: plan.height,
        fps,
        format: Pixel::YUV420P,
    };

    let work = WorkDir::new()?;
    let stats = work.0.join("x264.log");
    let mut part = PartFile::new(output);

    progress(0.0);
    let mix = mix_audio(input, plan.range, gains, cancel)?;

    // Pass 1: video only, packets dropped; x264 writes its stats.
    {
        let mut enc = encoders::x264_two_pass(&spec, plan.video_kbps, Pass::First, &stats, true)?;
        let mut packet = Packet::empty();
        run_video(
            input,
            spec,
            overlay.as_deref_mut(),
            cancel,
            &mut |frame| {
                enc.send(frame)?;
                while enc.receive(&mut packet) {}
                Ok(())
            },
            &mut |p| progress(p * 0.5),
        )?;
        enc.send_eof()?;
        while enc.receive(&mut packet) {}
    }

    // Pass 2: video and audio, muxed.
    encode_file(
        input,
        &part.path,
        "mp4",
        spec,
        &mut |global_header| {
            encoders::x264_two_pass(&spec, plan.video_kbps, Pass::Second, &stats, global_header)
        },
        mix.as_ref().map(|mix| (mix, plan.audio_kbps)),
        overlay,
        cancel,
        &mut |p| progress(0.5 + p * 0.5),
    )?;

    part.commit(output)?;
    progress(1.0);
    Ok(())
}

/// The frames an export encodes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct VideoSpec {
    /// The part of the original, in seconds.
    pub range: Cut,
    pub width: u32,
    pub height: u32,
    pub fps: Rational,
    /// The pixel format the encoder takes.
    pub format: Pixel,
}

impl VideoSpec {
    pub fn total_frames(&self) -> i64 {
        ((self.range.duration() * f64::from(self.fps)).round() as i64).max(1)
    }
}

/// Encodes the video of `spec` with the encoder `open_video` opens (told
/// whether the container wants global headers), muxes it with `audio` (the
/// mix and its AAC bitrate) and writes it to `path` as `container`.
/// `spec.format` is replaced by the encoder's. Returns the encoder's name.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_file(
    input: &Path,
    path: &Path,
    container: &str,
    mut spec: VideoSpec,
    open_video: &mut dyn FnMut(bool) -> Result<VideoEncoder, MediaError>,
    audio: Option<(&[Vec<f32>; 2], u32)>,
    overlay: Option<&mut (dyn Overlay + '_)>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<String, MediaError> {
    let mut octx = ffmpeg::format::output_as(path, container)
        .context(|| format!("can't create {}", path.display()))?;
    let global_header = octx
        .format()
        .flags()
        .contains(ffmpeg::format::Flags::GLOBAL_HEADER);

    let mut venc = open_video(global_header)?;
    spec.format = venc.format;
    let venc_tb = venc.time_base();
    let v_index = {
        let mut ost = octx
            .add_stream(venc.codec)
            .context(|| "can't add the video stream".into())?;
        ost.set_parameters(venc.encoder());
        ost.set_time_base(venc_tb);
        ost.index()
    };

    let mut audio = match audio {
        Some((mix, kbps)) => {
            let (mut aenc, acodec) = open_audio_encoder(kbps, global_header)?;
            let index = {
                let mut ost = octx
                    .add_stream(acodec)
                    .context(|| "can't add the audio stream".into())?;
                ost.set_parameters(&aenc);
                ost.set_time_base((1, RATE));
                ost.index()
            };
            let packets = encode_audio(&mut aenc, acodec, mix)?;
            Some(AudioOut {
                index,
                packets: packets.into(),
            })
        }
        None => None,
    };

    let mut options = Dictionary::new();
    if matches!(container, "mp4" | "mov") {
        options.set("movflags", "+faststart");
    }
    octx.write_header_with(options)
        .context(|| format!("can't write {}", path.display()))?;
    let v_tb = octx.stream(v_index).expect("added above").time_base();
    let a_tb = audio
        .as_ref()
        .map(|a| octx.stream(a.index).expect("added above").time_base());

    let mux = |enc: &mut VideoEncoder,
               octx: &mut ffmpeg::format::context::Output,
               audio: &mut Option<AudioOut>|
     -> Result<(), MediaError> {
        let mut packet = Packet::empty();
        while enc.receive(&mut packet) {
            let t = packet.dts().unwrap_or(0) as f64 * f64::from(venc_tb);
            if let (Some(audio), Some(a_tb)) = (audio.as_mut(), a_tb) {
                audio.write_until(t, a_tb, octx)?;
            }
            packet.set_stream(v_index);
            // One frame long, so the last frame keeps its duration.
            packet.set_duration(1);
            packet.rescale_ts(venc_tb, v_tb);
            packet
                .write_interleaved(octx)
                .context(|| "can't write a video packet".into())?;
        }
        Ok(())
    };

    run_video(
        input,
        spec,
        overlay,
        cancel,
        &mut |frame| {
            venc.send(frame)?;
            mux(&mut venc, &mut octx, &mut audio)
        },
        progress,
    )?;
    venc.send_eof()?;
    mux(&mut venc, &mut octx, &mut audio)?;
    if let (Some(audio), Some(a_tb)) = (audio.as_mut(), a_tb) {
        audio.write_until(f64::INFINITY, a_tb, &mut octx)?;
    }
    octx.write_trailer()
        .context(|| format!("can't finish {}", path.display()))?;
    Ok(venc.name.to_string())
}

pub(crate) fn open_audio_encoder(
    kbps: u32,
    global_header: bool,
) -> Result<(encoder::audio::Encoder, ffmpeg::Codec), MediaError> {
    // FFmpeg's own AAC encoder: present in every build, unlike fdk-aac.
    let codec = encoder::find_by_name("aac").ok_or(MediaError::NoEncoder("aac"))?;
    let mut enc = codec::context::Context::new_with_codec(codec)
        .encoder()
        .audio()
        .context(|| "can't set up the AAC encoder".into())?;
    enc.set_rate(RATE);
    enc.set_channel_layout(ChannelLayout::STEREO);
    enc.set_format(PLANAR_F32);
    enc.set_bit_rate(kbps as usize * 1000);
    enc.set_time_base((1, RATE));
    if global_header {
        enc.set_flags(codec::Flags::GLOBAL_HEADER);
    }
    let enc = enc
        .open_as(codec)
        .context(|| "can't open the AAC encoder".into())?;
    Ok((enc, codec))
}

/// Encoded audio waiting to be interleaved with the video.
struct AudioOut {
    index: usize,
    /// Packets in 1/48000 s.
    packets: VecDeque<Packet>,
}

impl AudioOut {
    /// Writes the audio packets that start before `t` seconds.
    fn write_until(
        &mut self,
        t: f64,
        tb: Rational,
        octx: &mut ffmpeg::format::context::Output,
    ) -> Result<(), MediaError> {
        while let Some(packet) = self.packets.front() {
            let start = packet.dts().unwrap_or(0) as f64 / f64::from(RATE);
            if start > t {
                break;
            }
            let mut packet = self.packets.pop_front().expect("front exists");
            packet.set_stream(self.index);
            packet.rescale_ts((1, RATE), tb);
            packet
                .write_interleaved(octx)
                .context(|| "can't write an audio packet".into())?;
        }
        Ok(())
    }
}

fn encode_audio(
    enc: &mut encoder::audio::Encoder,
    codec: ffmpeg::Codec,
    mix: &[Vec<f32>; 2],
) -> Result<Vec<Packet>, MediaError> {
    let frame_size = match enc.frame_size() as usize {
        0 => 1024,
        n => n,
    };
    let small_last = codec
        .capabilities()
        .contains(Capabilities::SMALL_LAST_FRAME);
    let mut packets = Vec::new();
    let drain = |enc: &mut encoder::audio::Encoder, packets: &mut Vec<Packet>| {
        let mut packet = Packet::empty();
        while enc.receive_packet(&mut packet).is_ok() {
            packets.push(std::mem::replace(&mut packet, Packet::empty()));
        }
    };

    let total = mix[0].len();
    let mut pos = 0;
    while pos < total {
        let len = frame_size.min(total - pos);
        let alloc = if len < frame_size && !small_last {
            frame_size
        } else {
            len
        };
        let mut frame = frame::Audio::new(PLANAR_F32, alloc, ChannelLayout::STEREO);
        frame.set_rate(RATE as u32);
        for (ch, samples) in mix.iter().enumerate() {
            let plane = frame.plane_mut::<f32>(ch);
            plane[..len].copy_from_slice(&samples[pos..pos + len]);
            plane[len..].fill(0.0);
        }
        frame.set_pts(Some(pos as i64));
        enc.send_frame(&frame)
            .context(|| "the AAC encoder refused a frame".into())?;
        drain(enc, &mut packets);
        pos += len;
    }
    enc.send_eof()
        .context(|| "can't finish the AAC encoder".into())?;
    drain(enc, &mut packets);
    Ok(packets)
}

/// The start time of the file, in seconds: timestamps are relative to it,
/// as the plan's range is.
pub(crate) fn start_offset(ictx: &ffmpeg::format::context::Input) -> f64 {
    // SAFETY: reads a plain field of the open context.
    let start = unsafe { (*ictx.as_ptr()).start_time };
    if start == ffmpeg::ffi::AV_NOPTS_VALUE {
        0.0
    } else {
        start as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE)
    }
}

/// Seeks to the keyframe at or before `seconds` (in file time).
pub(crate) fn seek_before(
    ictx: &mut ffmpeg::format::context::Input,
    seconds: f64,
) -> Result<(), MediaError> {
    let ts = (seconds * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
    ictx.seek(ts, ..ts).context(|| "can't seek".into())
}

/// [`MediaError::Cancelled`] once `cancel` is set.
pub(crate) fn check(cancel: &AtomicBool) -> Result<(), MediaError> {
    if cancel.load(Ordering::Relaxed) {
        Err(MediaError::Cancelled)
    } else {
        Ok(())
    }
}

pub(crate) fn file_size(path: &Path) -> Result<u64, MediaError> {
    fs::metadata(path)
        .map(|m| m.len())
        .map_err(|source| MediaError::Io {
            action: "can't read",
            path: path.into(),
            source,
        })
}

/// Decodes the video of `spec.range` and hands `emit` exactly
/// `spec.total_frames()` frames, scaled, in `spec.format`, with pts 0, 1,
/// 2… at `spec.fps`. Each output slot gets the last source frame shown at
/// or before its time, which drops or repeats frames as the frame rate
/// change needs. With an `overlay`, each frame goes through RGBA to be
/// drawn on, at the slot's time in the original. Checks `cancel` before
/// every frame.
pub(crate) fn run_video(
    input: &Path,
    spec: VideoSpec,
    overlay: Option<&mut (dyn Overlay + '_)>,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(&frame::Video) -> Result<(), MediaError>,
    progress: &mut dyn FnMut(f32),
) -> Result<(), MediaError> {
    let mut ictx =
        ffmpeg::format::input(input).context(|| format!("can't open {}", input.display()))?;
    let stream = ictx
        .streams()
        .best(media::Type::Video)
        .ok_or_else(|| MediaError::NoVideo(input.into()))?;
    let index = stream.index();
    let tb = stream.time_base();
    let offset = start_offset(&ictx);

    let mut ctx = codec::context::Context::from_parameters(stream.parameters())
        .context(|| "can't read the video stream".into())?;
    ctx.set_threading(threading::Config {
        kind: threading::Type::Frame,
        count: 0,
    });
    let mut decoder = ctx
        .decoder()
        .video()
        .context(|| "can't open the video decoder".into())?;

    if spec.range.start > 0.0 {
        seek_before(&mut ictx, spec.range.start + offset)?;
    }

    let mut slots = Slots {
        start: spec.range.start,
        fps: f64::from(spec.fps),
        total: spec.total_frames(),
        next: 0,
        width: spec.width,
        height: spec.height,
        format: spec.format,
        overlay: overlay.filter(|o| !o.is_empty()),
        cancel,
        held: None,
        scaled: None,
        scaler: None,
        back: None,
        canvas: RgbaImage::default(),
        last_t: None,
    };

    let mut decoded = frame::Video::empty();
    let mut receive = |decoder: &mut ffmpeg::decoder::Video,
                       slots: &mut Slots,
                       emit: &mut dyn FnMut(&frame::Video) -> Result<(), MediaError>,
                       progress: &mut dyn FnMut(f32)|
     -> Result<bool, MediaError> {
        while decoder.receive_frame(&mut decoded).is_ok() {
            let t = decoded
                .timestamp()
                .map(|ts| ts as f64 * f64::from(tb) - offset);
            let frame = std::mem::replace(&mut decoded, frame::Video::empty());
            if slots.push(frame, t, emit, progress)? {
                return Ok(true);
            }
        }
        Ok(false)
    };

    let mut done = false;
    for (s, packet) in ictx.packets() {
        if s.index() != index {
            continue;
        }
        check(cancel)?;
        decoder
            .send_packet(&packet)
            .context(|| "can't decode the video".into())?;
        if receive(&mut decoder, &mut slots, emit, progress)? {
            done = true;
            break;
        }
    }
    if !done {
        decoder
            .send_eof()
            .context(|| "can't finish decoding the video".into())?;
        receive(&mut decoder, &mut slots, emit, progress)?;
    }
    slots.finish(emit, progress)
}

/// The output frame slots and the source frame currently shown.
struct Slots<'a, 'o> {
    start: f64,
    fps: f64,
    total: i64,
    next: i64,
    width: u32,
    height: u32,
    format: Pixel,
    overlay: Option<&'a mut (dyn Overlay + 'o)>,
    cancel: &'a AtomicBool,
    /// The latest decoded frame, not yet scaled.
    held: Option<frame::Video>,
    /// `held`, scaled (to RGBA when there's an overlay, else to `format`),
    /// once a slot has needed it.
    scaled: Option<frame::Video>,
    scaler: Option<scaling::Context>,
    /// RGBA to `format`, after the overlay is drawn.
    back: Option<scaling::Context>,
    /// What the overlay draws on.
    canvas: RgbaImage,
    last_t: Option<f64>,
}

impl Slots<'_, '_> {
    fn slot_time(&self, n: i64) -> f64 {
        self.start + n as f64 / self.fps
    }

    /// Takes the next decoded frame, shown from `t` seconds. Fills every
    /// slot before `t` with the previous frame. True once every slot is
    /// filled.
    fn push(
        &mut self,
        frame: frame::Video,
        t: Option<f64>,
        emit: &mut dyn FnMut(&frame::Video) -> Result<(), MediaError>,
        progress: &mut dyn FnMut(f32),
    ) -> Result<bool, MediaError> {
        let t = t.unwrap_or_else(|| self.last_t.map_or(0.0, |last| last + 1.0 / self.fps));
        self.last_t = Some(t);
        if self.held.is_some() {
            // A microsecond of slack, so a frame exactly on a slot's time
            // is the one shown in it.
            while self.next < self.total && self.slot_time(self.next) < t - 1e-6 {
                self.emit_held(emit, progress)?;
            }
        }
        self.held = Some(frame);
        self.scaled = None;
        Ok(self.next >= self.total)
    }

    /// Fills the remaining slots with the last frame.
    fn finish(
        &mut self,
        emit: &mut dyn FnMut(&frame::Video) -> Result<(), MediaError>,
        progress: &mut dyn FnMut(f32),
    ) -> Result<(), MediaError> {
        if self.held.is_none() {
            return if self.next > 0 {
                Ok(())
            } else {
                Err(MediaError::EmptyRange)
            };
        }
        while self.next < self.total {
            self.emit_held(emit, progress)?;
        }
        Ok(())
    }

    /// Scales `held` to the export's size, in `format`.
    fn scale_held(&mut self, format: Pixel) -> Result<frame::Video, MediaError> {
        let held = self.held.as_ref().expect("callers check");
        let fits = self.scaler.as_ref().is_some_and(|s| {
            s.input().format == held.format()
                && s.input().width == held.width()
                && s.input().height == held.height()
        });
        if !fits {
            let mut scaler = scaling::Context::get(
                held.format(),
                held.width(),
                held.height(),
                format,
                self.width,
                self.height,
                scaling::Flags::BICUBIC,
            )
            .context(|| "can't set up scaling".into())?;
            if format == Pixel::RGBA {
                composite::yuv_to_rgb_colors(&mut scaler, held);
            }
            self.scaler = Some(scaler);
        }
        // A fresh frame each time: the encoder may still hold the previous
        // one.
        let mut scaled = frame::Video::new(format, self.width, self.height);
        self.scaler
            .as_mut()
            .expect("set above")
            .run(held, &mut scaled)
            .context(|| "can't scale a frame".into())?;
        Ok(scaled)
    }

    fn emit_held(
        &mut self,
        emit: &mut dyn FnMut(&frame::Video) -> Result<(), MediaError>,
        progress: &mut dyn FnMut(f32),
    ) -> Result<(), MediaError> {
        check(self.cancel)?;
        let t = self.slot_time(self.next);
        let draws = self.overlay.is_some();
        if self.scaled.is_none() {
            let format = if draws { Pixel::RGBA } else { self.format };
            self.scaled = Some(self.scale_held(format)?);
        }

        if !draws {
            // Shared with the next slots showing the same frame: the encoder
            // keeps its own reference to the pixels.
            let scaled = self.scaled.as_mut().expect("set above");
            scaled.set_pts(Some(self.next));
            scaled.set_kind(ffmpeg::picture::Type::None);
            emit(scaled)?;
            self.next += 1;
            progress(self.next as f32 / self.total as f32);
            return Ok(());
        }

        let mut frame = {
            let scaled = self.scaled.as_ref().expect("set above");
            composite::frame_to_image(scaled, &mut self.canvas);
            self.overlay
                .as_deref_mut()
                .expect("checked above")
                .draw(t, &mut self.canvas);
            let mut rgba = frame::Video::new(Pixel::RGBA, self.width, self.height);
            composite::image_to_frame(&self.canvas, &mut rgba);
            if self.format == Pixel::RGBA {
                rgba
            } else {
                if self.back.is_none() {
                    let mut back = scaling::Context::get(
                        Pixel::RGBA,
                        self.width,
                        self.height,
                        self.format,
                        self.width,
                        self.height,
                        scaling::Flags::BICUBIC,
                    )
                    .context(|| "can't set up the colour conversion".into())?;
                    let held = self.held.as_ref().expect("callers check");
                    composite::rgb_to_yuv_colors(&mut back, held);
                    self.back = Some(back);
                }
                let mut out = frame::Video::new(self.format, self.width, self.height);
                self.back
                    .as_mut()
                    .expect("set above")
                    .run(&rgba, &mut out)
                    .context(|| "can't convert a frame".into())?;
                out
            }
        };
        frame.set_pts(Some(self.next));
        frame.set_kind(ffmpeg::picture::Type::None);
        emit(&frame)?;
        self.next += 1;
        progress(self.next as f32 / self.total as f32);
        Ok(())
    }
}

/// One audio track being decoded into the mix.
struct Track {
    stream: usize,
    gain: f32,
    tb: Rational,
    decoder: ffmpeg::decoder::Audio,
    resampler: Option<resampling::Context>,
    /// Where the next resampled sample goes, in samples from the range's
    /// start.
    pos: Option<i64>,
    done: bool,
}

/// Decodes every audio track of `range` and mixes them, with their gains,
/// into one stereo 48 kHz buffer. `None` when the clip has no audio.
pub(crate) fn mix_audio(
    input: &Path,
    range: Cut,
    gains: &[f32],
    cancel: &AtomicBool,
) -> Result<Option<[Vec<f32>; 2]>, MediaError> {
    let mut ictx =
        ffmpeg::format::input(input).context(|| format!("can't open {}", input.display()))?;
    let offset = start_offset(&ictx);
    let total = (range.duration() * f64::from(RATE)).round() as usize;

    let mut tracks = Vec::new();
    let mut any_audio = false;
    let audio_streams = ictx
        .streams()
        .filter(|s| s.parameters().medium() == media::Type::Audio);
    for (n, stream) in audio_streams.enumerate() {
        any_audio = true;
        let gain = gains.get(n).copied().unwrap_or(1.0);
        if gain == 0.0 {
            continue;
        }
        let decoder = codec::context::Context::from_parameters(stream.parameters())
            .and_then(|c| c.decoder().audio())
            .context(|| format!("can't open audio track {}", n + 1))?;
        tracks.push(Track {
            stream: stream.index(),
            gain,
            tb: stream.time_base(),
            decoder,
            resampler: None,
            pos: None,
            done: false,
        });
    }
    if !any_audio {
        return Ok(None);
    }

    let mut mix = [vec![0.0f32; total], vec![0.0f32; total]];
    if !tracks.is_empty() {
        if range.start > 0.0 {
            seek_before(&mut ictx, range.start + offset)?;
        }
        let start = range.start;
        for (s, packet) in ictx.packets() {
            let Some(track) = tracks.iter_mut().find(|t| t.stream == s.index()) else {
                continue;
            };
            if track.done {
                continue;
            }
            check(cancel)?;
            track
                .decoder
                .send_packet(&packet)
                .context(|| "can't decode the audio".into())?;
            drain_audio(track, &mut mix, start, offset)?;
            if tracks.iter().all(|t| t.done) {
                break;
            }
        }
        for track in tracks.iter_mut().filter(|t| !t.done) {
            track
                .decoder
                .send_eof()
                .context(|| "can't finish decoding the audio".into())?;
            drain_audio(track, &mut mix, start, offset)?;
        }
    }
    check(cancel)?;

    let fade = ((EDGE_FADE_SECS * f64::from(RATE)) as usize).min(total / 2);
    for channel in &mut mix {
        for i in 0..fade {
            let g = i as f32 / fade as f32;
            channel[i] *= g;
            channel[total - 1 - i] *= g;
        }
        for s in channel.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }
    }
    Ok(Some(mix))
}

/// Takes the decoded frames of `track`, resamples them and adds them to
/// the mix.
fn drain_audio(
    track: &mut Track,
    mix: &mut [Vec<f32>; 2],
    start: f64,
    offset: f64,
) -> Result<(), MediaError> {
    let total = mix[0].len() as i64;
    let mut decoded = frame::Audio::empty();
    while track.decoder.receive_frame(&mut decoded).is_ok() {
        if track.done {
            continue;
        }
        if track.resampler.is_none() {
            let mut layout = decoded.channel_layout();
            if layout.0.order == ffmpeg::ffi::AVChannelOrder::AV_CHANNEL_ORDER_UNSPEC {
                layout = ChannelLayout::default(layout.channels());
            }
            track.resampler = Some(
                resampling::Context::get(
                    decoded.format(),
                    layout,
                    decoded.rate(),
                    PLANAR_F32,
                    ChannelLayout::STEREO,
                    RATE as u32,
                )
                .context(|| "can't set up audio resampling".into())?,
            );
        }
        let resampler = track.resampler.as_mut().expect("set above");

        // Room for the resampled samples plus what the resampler held back.
        let room = decoded.samples() as u64 * RATE as u64 / u64::from(decoded.rate().max(1)) + 256;
        let mut out = frame::Audio::new(PLANAR_F32, room as usize, ChannelLayout::STEREO);
        resampler
            .run(&decoded, &mut out)
            .context(|| "can't resample the audio".into())?;

        // Place the samples by timestamp, then contiguously; resync when
        // the timestamps jump by more than 100 ms.
        let at = decoded.timestamp().map(|ts| {
            ((ts as f64 * f64::from(track.tb) - offset - start) * f64::from(RATE)).round() as i64
        });
        let pos = match (track.pos, at) {
            (Some(pos), Some(at)) if (pos - at).abs() > i64::from(RATE / 10) => at,
            (Some(pos), _) => pos,
            (None, Some(at)) => at,
            (None, None) => 0,
        };

        let n = out.samples();
        for (ch, channel) in mix.iter_mut().enumerate() {
            let samples = out.plane::<f32>(ch);
            for (i, &s) in samples.iter().enumerate().take(n) {
                let idx = pos + i as i64;
                if (0..total).contains(&idx) {
                    channel[idx as usize] += s * track.gain;
                }
            }
        }
        track.pos = Some(pos + n as i64);
        if pos + n as i64 >= total {
            track.done = true;
        }
    }
    Ok(())
}

/// A temporary folder for x264's stats, removed when dropped.
pub(crate) struct WorkDir(pub PathBuf);

impl WorkDir {
    pub fn new() -> Result<WorkDir, MediaError> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "montage-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).map_err(|source| MediaError::Io {
            action: "can't create",
            path: path.clone(),
            source,
        })?;
        Ok(WorkDir(path))
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The export being written, next to its final name, and removed unless
/// committed.
pub(crate) struct PartFile {
    pub path: PathBuf,
    committed: bool,
}

impl PartFile {
    pub fn new(output: &Path) -> PartFile {
        let name = output
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "export".into());
        PartFile {
            path: output.with_file_name(format!(".{name}.part")),
            committed: false,
        }
    }

    pub fn commit(&mut self, output: &Path) -> Result<(), MediaError> {
        fs::rename(&self.path, output).map_err(|source| MediaError::Io {
            action: "can't move the export to",
            path: output.into(),
            source,
        })?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}
