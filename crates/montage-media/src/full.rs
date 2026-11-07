//! The Full quality export: the original's size and frame rate. The video
//! stream is copied as is when only the cut changed and the cut starts on a
//! keyframe; otherwise it's re-encoded with the first encoder of
//! [`encoders::FULL_QUALITY`] that works.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use ffmpeg_next as ffmpeg;
use ffmpeg_next::format::Pixel;
use ffmpeg_next::{Dictionary, Rational, encoder, ffi, media};
use montage_core::edit::Cut;
use montage_core::render::Overlay;

use crate::encoders::{self, X264_FULL_CRF};
use crate::export::{
    PartFile, VideoSpec, check, encode_file, mix_audio, seek_before, start_offset,
};
use crate::{Context, MediaError, MediaInfo, init};

/// The mixed-down audio's bitrate, in kbit/s.
pub(crate) const AUDIO_KBPS: u32 = 192;

/// Exports `cut` of `input` at full quality. Returns how: `stream copy`,
/// or the encoder's name.
pub(crate) fn export_full(
    input: &Path,
    output: &Path,
    cut: Cut,
    gains: &[f32],
    overlay: Option<&mut (dyn Overlay + '_)>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<String, MediaError> {
    init();
    let info = crate::probe(input)?;
    let cut = cut.clamped(info.duration);
    if cut.duration() <= 0.0 {
        return Err(MediaError::EmptyRange);
    }
    let container = container_for(output);
    let mut part = PartFile::new(output);
    let mut overlay = overlay.filter(|o| !o.is_empty());
    progress(0.0);

    let copyable = overlay.is_none() && gains.iter().all(|&g| g == 1.0);
    if copyable && let Some(zero) = keyframe_at(input, cut.start, info.fps)? {
        match stream_copy(
            input, &part.path, container, cut, zero, info.fps, cancel, progress,
        ) {
            Ok(()) => {
                part.commit(output)?;
                progress(1.0);
                return Ok("stream copy".into());
            }
            Err(MediaError::Cancelled) => return Err(MediaError::Cancelled),
            // The container can't hold one of the streams as is.
            Err(_) => {}
        }
    }

    let fps = Rational::from(if info.fps > 0.0 { info.fps } else { 30.0 }).reduce();
    let even = |v: u32| (v & !1).max(2);
    let spec = VideoSpec {
        range: cut,
        width: even(info.width),
        height: even(info.height),
        fps,
        format: Pixel::YUV420P,
    };
    let mix = mix_audio(input, cut, gains, cancel)?;
    let bit_rate = video_bit_rate(input, &info);

    let mut last = MediaError::NoEncoder("libx264");
    for &name in encoders::FULL_QUALITY {
        if encoder::find_by_name(name).is_none() {
            continue;
        }
        let result = encode_file(
            input,
            &part.path,
            container,
            spec,
            &mut |global_header| encoders::full_quality(name, &spec, bit_rate, global_header),
            mix.as_ref().map(|mix| (mix, AUDIO_KBPS)),
            overlay.as_deref_mut(),
            cancel,
            progress,
        );
        match result {
            Ok(name) => {
                part.commit(output)?;
                progress(1.0);
                return Ok(if name == "libx264" {
                    format!("libx264 crf {X264_FULL_CRF}")
                } else {
                    name
                });
            }
            Err(MediaError::Cancelled) => return Err(MediaError::Cancelled),
            // A GPU encoder that didn't open, or failed once started: the
            // next one starts over.
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// The muxer for `output`, from its extension.
fn container_for(output: &Path) -> &'static str {
    let ext = output
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "mov" => "mov",
        "mkv" => "matroska",
        "webm" => "webm",
        _ => "mp4",
    }
}

/// When `start` (seconds) falls on a video keyframe, within half a frame,
/// the keyframe's time, which becomes the export's zero. The clip's own
/// start counts as a keyframe, with zero at 0.
pub(crate) fn keyframe_at(input: &Path, start: f64, fps: f64) -> Result<Option<f64>, MediaError> {
    let mut ictx =
        ffmpeg::format::input(input).context(|| format!("can't open {}", input.display()))?;
    let offset = start_offset(&ictx);
    let stream = ictx
        .streams()
        .best(media::Type::Video)
        .ok_or_else(|| MediaError::NoVideo(input.into()))?;
    let (index, tb) = (stream.index(), stream.time_base());
    let half = half_frame(fps);
    let from_start = start <= half;
    if !from_start {
        seek_before(&mut ictx, start + offset + half)?;
    }
    for (s, packet) in ictx.packets() {
        if s.index() != index {
            continue;
        }
        let Some(ts) = packet.pts().or(packet.dts()) else {
            continue;
        };
        let t = ts as f64 * f64::from(tb) - offset;
        if packet.is_key() {
            if from_start {
                return Ok(Some(0.0));
            }
            return Ok(((t - start).abs() <= half).then_some(t));
        }
        if t > start + half {
            break;
        }
    }
    Ok(None)
}

fn half_frame(fps: f64) -> f64 {
    0.5 / if fps > 0.0 { fps } else { 30.0 }
}

/// Copies the video packets of `cut` from the keyframe at `zero`, and
/// every audio track's packets of the same span, unchanged, with
/// timestamps moved so `zero` is the start.
#[allow(clippy::too_many_arguments)]
fn stream_copy(
    input: &Path,
    path: &Path,
    container: &str,
    cut: Cut,
    zero: f64,
    fps: f64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<(), MediaError> {
    let mut ictx =
        ffmpeg::format::input(input).context(|| format!("can't open {}", input.display()))?;
    let offset = start_offset(&ictx);
    let video = ictx
        .streams()
        .best(media::Type::Video)
        .ok_or_else(|| MediaError::NoVideo(input.into()))?
        .index();
    let half = half_frame(fps);
    let from_start = cut.start <= half;

    let mut octx = ffmpeg::format::output_as(path, container)
        .context(|| format!("can't create {}", path.display()))?;
    // SAFETY: reads a static table pointer of the open muxer.
    let tags = unsafe { (*(*octx.as_ptr()).oformat).codec_tag };
    let mut map = vec![None; ictx.nb_streams() as usize];
    for ist in ictx.streams() {
        if ist.index() != video && ist.parameters().medium() != media::Type::Audio {
            continue;
        }
        let mut ost = octx
            .add_stream(None::<ffmpeg::Codec>)
            .context(|| "can't add a stream".into())?;
        ost.set_parameters(ist.parameters());
        ost.set_time_base(ist.time_base());
        // Keep the source's codec tag (`hvc1`…) only when the container
        // knows it for the same codec, as ffmpeg does.
        // SAFETY: plain fields of the stream's own parameters.
        unsafe {
            let par = (*ost.as_mut_ptr()).codecpar;
            let tag = (*par).codec_tag;
            if tag != 0 && !tags.is_null() && ffi::av_codec_get_id(tags, tag) != (*par).codec_id {
                (*par).codec_tag = 0;
            }
        }
        map[ist.index()] = Some(ost.index());
    }

    let mut options = Dictionary::new();
    if matches!(container, "mp4" | "mov") {
        options.set("movflags", "+faststart");
    }
    octx.write_header_with(options)
        .context(|| format!("can't write {}", path.display()))?;
    let out_tb: Vec<Rational> = octx.streams().map(|s| s.time_base()).collect();

    if !from_start {
        seek_before(&mut ictx, cut.start + offset + half)?;
    }
    let (mut started, mut video_done) = (false, false);
    for (s, mut packet) in ictx.packets() {
        check(cancel)?;
        let Some(out) = map[s.index()] else {
            continue;
        };
        let tb = s.time_base();
        let Some(ts) = packet.pts().or(packet.dts()) else {
            continue;
        };
        let seconds = |ts: i64| ts as f64 * f64::from(tb) - offset;
        let t = seconds(ts);
        let dts = packet.dts().map_or(t, seconds);
        if dts > cut.end + 1.0 {
            // Past the end on every stream, with room for interleaving.
            break;
        }
        if s.index() == video {
            if video_done {
                continue;
            }
            if !started {
                if !packet.is_key() || (!from_start && t < zero - half) {
                    continue;
                }
                started = true;
            }
            if t >= cut.end - half {
                // In decode order: once the decode time is past the end,
                // so is every later frame.
                video_done |= dts >= cut.end - half;
                continue;
            }
            progress((((t - cut.start) / cut.duration()).clamp(0.0, 1.0) * 0.99) as f32);
        } else if (!from_start && t < zero - 1e-3) || t >= cut.end {
            continue;
        }

        let shift = ((zero + offset) / f64::from(tb)).round() as i64;
        packet.set_pts(packet.pts().map(|p| p - shift));
        packet.set_dts(packet.dts().map(|d| d - shift));
        packet.set_position(-1);
        packet.set_stream(out);
        packet.rescale_ts(tb, out_tb[out]);
        packet
            .write_interleaved(&mut octx)
            .context(|| "can't write a packet".into())?;
    }
    if !started {
        return Err(MediaError::EmptyRange);
    }
    octx.write_trailer()
        .context(|| format!("can't finish {}", path.display()))?;
    Ok(())
}

/// The source video's bitrate in bit/s: the stream's, else the file's less
/// a guess for the audio.
fn video_bit_rate(input: &Path, info: &MediaInfo) -> u64 {
    let stream_rate = ffmpeg::format::input(input).ok().and_then(|ictx| {
        let rate = ictx
            .streams()
            .best(media::Type::Video)?
            .parameters()
            .bit_rate();
        (rate > 0).then_some(rate as u64)
    });
    stream_rate.unwrap_or_else(|| {
        info.bit_rate
            .saturating_sub(info.audio_tracks as u64 * 160_000)
    })
}
