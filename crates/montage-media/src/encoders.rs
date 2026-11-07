//! The video encoders: libx264's two passes for Share, and for Full quality
//! the first GPU encoder that opens, with libx264 as the fallback.

use std::path::Path;
use std::ptr;

use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec;
use ffmpeg_next::ffi;
use ffmpeg_next::format::Pixel;
use ffmpeg_next::{Dictionary, Packet, Rational, encoder, frame};

use crate::export::VideoSpec;
use crate::{Context, MediaError};

/// x264's speed and quality trade-off for Share. Both passes use it; the
/// first pass is sped up further by libav's default `fastfirstpass`.
const X264_SHARE_PRESET: &str = "medium";

/// Full quality's encoders, tried in this order: NVIDIA, AMD, Intel, VAAPI
/// (Linux, any vendor), Apple, then software.
pub(crate) const FULL_QUALITY: &[&str] = &[
    "h264_nvenc",
    "h264_amf",
    "h264_qsv",
    "h264_vaapi",
    "h264_videotoolbox",
    "libx264",
];

/// libx264's constant quality for Full quality: visually lossless for game
/// footage.
pub(crate) const X264_FULL_CRF: u32 = 18;

#[derive(Clone, Copy)]
pub(crate) enum Pass {
    First,
    Second,
}

/// An open video encoder and what it takes.
pub(crate) struct VideoEncoder {
    enc: encoder::video::Encoder,
    pub codec: ffmpeg::Codec,
    pub name: &'static str,
    /// The pixel format of the frames [`VideoEncoder::send`] takes.
    pub format: Pixel,
    /// Set for encoders that take GPU frames (VAAPI): frames are uploaded
    /// first.
    hw: Option<HwFrames>,
}

impl VideoEncoder {
    pub fn encoder(&self) -> &encoder::video::Encoder {
        &self.enc
    }

    pub fn time_base(&self) -> Rational {
        self.enc.time_base()
    }

    pub fn send(&mut self, frame: &frame::Video) -> Result<(), MediaError> {
        let name = self.name;
        match &self.hw {
            None => self.enc.send_frame(frame),
            Some(hw) => {
                let uploaded = hw.upload(frame)?;
                self.enc.send_frame(&uploaded)
            }
        }
        .context(|| format!("{name} refused a frame"))
    }

    pub fn send_eof(&mut self) -> Result<(), MediaError> {
        let name = self.name;
        self.enc
            .send_eof()
            .context(|| format!("can't finish {name}"))
    }

    /// The next encoded packet, if one is ready.
    pub fn receive(&mut self, packet: &mut Packet) -> bool {
        self.enc.receive_packet(packet).is_ok()
    }
}

/// A video encoder context for `spec`, not yet opened.
fn setup(
    codec: ffmpeg::Codec,
    spec: &VideoSpec,
    format: Pixel,
    global_header: bool,
) -> Result<encoder::video::Video, MediaError> {
    let mut enc = codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .context(|| format!("can't set up {}", codec.name()))?;
    enc.set_width(spec.width);
    enc.set_height(spec.height);
    enc.set_format(format);
    enc.set_time_base(spec.fps.invert());
    enc.set_frame_rate(Some(spec.fps));
    if global_header {
        enc.set_flags(codec::Flags::GLOBAL_HEADER);
    }
    Ok(enc)
}

/// libx264 for one of Share's passes, at `kbps`, with its stats in `stats`.
pub(crate) fn x264_two_pass(
    spec: &VideoSpec,
    kbps: u32,
    pass: Pass,
    stats: &Path,
    global_header: bool,
) -> Result<VideoEncoder, MediaError> {
    let codec = encoder::find_by_name("libx264").ok_or(MediaError::NoEncoder("libx264"))?;
    let mut enc = setup(codec, spec, Pixel::YUV420P, false)?;
    enc.set_bit_rate(kbps as usize * 1000);
    let mut flags = match pass {
        Pass::First => codec::Flags::PASS1,
        Pass::Second => codec::Flags::PASS2,
    };
    if global_header {
        flags |= codec::Flags::GLOBAL_HEADER;
    }
    enc.set_flags(flags);

    let mut options = Dictionary::new();
    options.set("preset", X264_SHARE_PRESET);
    options.set("profile", "high");
    options.set("stats", &stats.to_string_lossy());
    let enc = enc
        .open_as_with(codec, options)
        .context(|| "can't open x264".into())?;
    Ok(VideoEncoder {
        enc,
        codec,
        name: "libx264",
        format: Pixel::YUV420P,
        hw: None,
    })
}

/// Opens `name`, one of [`FULL_QUALITY`], for a high quality encode of
/// `spec`. `bit_rate` (bit/s) is the source video's, for the encoders that
/// only do bitrate targets (VideoToolbox). Fails when this FFmpeg lacks
/// the encoder or the hardware isn't there.
pub(crate) fn full_quality(
    name: &'static str,
    spec: &VideoSpec,
    bit_rate: u64,
    global_header: bool,
) -> Result<VideoEncoder, MediaError> {
    let codec = encoder::find_by_name(name).ok_or(MediaError::NoEncoder(name))?;
    if name == "h264_vaapi" {
        return vaapi(codec, spec, global_header);
    }
    let format = software_format(codec);
    let mut enc = setup(codec, spec, format, global_header)?;
    // A keyframe every 2 s, so players seek quickly.
    enc.set_gop((f64::from(spec.fps) * 2.0).round().max(1.0) as u32);

    let mut options = Dictionary::new();
    match name {
        "h264_nvenc" => {
            // Constant quality: VBR with a quality target and no bitrate.
            options.set("preset", "p5");
            options.set("tune", "hq");
            options.set("rc", "vbr");
            options.set("cq", "20");
            options.set("spatial_aq", "1");
            options.set("profile", "high");
            enc.set_bit_rate(0);
        }
        "h264_amf" => {
            options.set("usage", "transcoding");
            options.set("quality", "quality");
            options.set("rc", "cqp");
            options.set("qp_i", "19");
            options.set("qp_p", "21");
            options.set("qp_b", "23");
            options.set("profile", "high");
        }
        "h264_qsv" => {
            // ICQ: a quality target, no bitrate.
            options.set("preset", "slower");
            options.set("global_quality", "20");
            options.set("profile", "high");
        }
        "h264_videotoolbox" => {
            // No constant quality mode on Intel Macs: aim at the source's
            // bitrate, with a floor for sources that were already squeezed.
            let floor = f64::from(spec.width) * f64::from(spec.height) * f64::from(spec.fps) * 0.08;
            let target = (bit_rate as f64).max(floor).min(200e6);
            enc.set_bit_rate(target as usize);
            options.set("profile", "high");
        }
        _ => {
            options.set("crf", &X264_FULL_CRF.to_string());
            options.set("preset", "fast");
            options.set("profile", "high");
        }
    }

    let hardware = name != "libx264";
    let enc = quiet_if(hardware, || enc.open_as_with(codec, options))
        .context(|| format!("can't open {name}"))?;
    Ok(VideoEncoder {
        enc,
        codec,
        name,
        format,
        hw: None,
    })
}

/// The software pixel format `codec` takes: yuv420p when it can, as the
/// scaler makes it directly, else NV12 (QSV).
fn software_format(codec: ffmpeg::Codec) -> Pixel {
    let formats: Vec<Pixel> = codec
        .video()
        .ok()
        .and_then(|v| v.formats())
        .map(|f| f.collect())
        .unwrap_or_default();
    if formats.is_empty() || formats.contains(&Pixel::YUV420P) {
        Pixel::YUV420P
    } else if formats.contains(&Pixel::NV12) {
        Pixel::NV12
    } else {
        formats[0]
    }
}

/// Runs `f` with libav's log turned down when `quiet`: a GPU encoder
/// failing to find its hardware logs an error, which is expected here.
fn quiet_if<T>(quiet: bool, f: impl FnOnce() -> T) -> T {
    if !quiet {
        return f();
    }
    ffmpeg::log::set_level(ffmpeg::log::Level::Fatal);
    let result = f();
    ffmpeg::log::set_level(ffmpeg::log::Level::Error);
    result
}

/// VAAPI (Linux, Intel and AMD): the encoder takes frames in GPU memory,
/// so each frame is uploaded from NV12 first.
fn vaapi(
    codec: ffmpeg::Codec,
    spec: &VideoSpec,
    global_header: bool,
) -> Result<VideoEncoder, MediaError> {
    if !cfg!(target_os = "linux") {
        return Err(MediaError::NoEncoder("h264_vaapi"));
    }
    let hw = quiet_if(true, || HwFrames::vaapi(spec.width, spec.height))?;
    let mut enc = setup(codec, spec, Pixel::VAAPI, global_header)?;
    enc.set_gop((f64::from(spec.fps) * 2.0).round().max(1.0) as u32);
    // SAFETY: the context is not open yet; it takes its own reference.
    unsafe {
        (*enc.as_mut_ptr()).hw_frames_ctx = ffi::av_buffer_ref(hw.0);
    }
    let mut options = Dictionary::new();
    options.set("rc_mode", "CQP");
    options.set("qp", "20");
    options.set("profile", "high");
    let enc = quiet_if(true, || enc.open_as_with(codec, options))
        .context(|| "can't open h264_vaapi".into())?;
    Ok(VideoEncoder {
        enc,
        codec,
        name: "h264_vaapi",
        format: Pixel::NV12,
        hw: Some(hw),
    })
}

/// A pool of GPU frames (an `AVHWFramesContext`), owned.
struct HwFrames(*mut ffi::AVBufferRef);

impl HwFrames {
    fn vaapi(width: u32, height: u32) -> Result<HwFrames, MediaError> {
        let fail = |what: &str, code: i32| MediaError::Ffmpeg {
            context: what.to_string(),
            source: ffmpeg::Error::from(code),
        };
        // SAFETY: libav's documented sequence to set up a frame pool; every
        // reference taken is released, by hand or by `Drop`.
        unsafe {
            let mut device = ptr::null_mut();
            let r = ffi::av_hwdevice_ctx_create(
                &mut device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                ptr::null(),
                ptr::null_mut(),
                0,
            );
            if r < 0 {
                return Err(fail("no VAAPI device", r));
            }
            let frames = ffi::av_hwframe_ctx_alloc(device);
            ffi::av_buffer_unref(&mut device);
            if frames.is_null() {
                return Err(fail("can't set up VAAPI frames", ffi::AVERROR(ffi::ENOMEM)));
            }
            let pool = HwFrames(frames);
            let ctx = (*frames).data as *mut ffi::AVHWFramesContext;
            (*ctx).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*ctx).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
            (*ctx).width = width as i32;
            (*ctx).height = height as i32;
            (*ctx).initial_pool_size = 20;
            let r = ffi::av_hwframe_ctx_init(frames);
            if r < 0 {
                return Err(fail("can't set up VAAPI frames", r));
            }
            Ok(pool)
        }
    }

    /// `frame` (NV12) copied into a GPU frame, with its timestamp.
    fn upload(&self, frame: &frame::Video) -> Result<frame::Video, MediaError> {
        let fail = |code: i32| MediaError::Ffmpeg {
            context: "can't upload a frame to the GPU".into(),
            source: ffmpeg::Error::from(code),
        };
        // SAFETY: the new frame is owned by `out`, which frees it.
        unsafe {
            let hw = ffi::av_frame_alloc();
            if hw.is_null() {
                return Err(fail(ffi::AVERROR(ffi::ENOMEM)));
            }
            let out = frame::Video::wrap(hw);
            let r = ffi::av_hwframe_get_buffer(self.0, hw, 0);
            if r < 0 {
                return Err(fail(r));
            }
            let r = ffi::av_hwframe_transfer_data(hw, frame.as_ptr(), 0);
            if r < 0 {
                return Err(fail(r));
            }
            (*hw).pts = (*frame.as_ptr()).pts;
            Ok(out)
        }
    }
}

impl Drop for HwFrames {
    fn drop(&mut self) {
        // SAFETY: releases the reference this owns.
        unsafe { ffi::av_buffer_unref(&mut self.0) }
    }
}
