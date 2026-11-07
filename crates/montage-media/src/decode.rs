//! Decoding shared by the player and the thumbnails: the video decoder,
//! on the GPU when the system has one (with a software fallback), frames
//! copied back to the CPU and converted to RGBA, and the audio tracks.

use std::ffi::{CString, c_void};
use std::path::Path;
use std::ptr;

use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec::{self, threading};
use ffmpeg_next::ffi;
use ffmpeg_next::format::{Pixel, Sample, sample};
use ffmpeg_next::software::{resampling, scaling};
use ffmpeg_next::{ChannelLayout, Packet, Rational, frame, media};
use montage_core::render::RgbaImage;

use crate::{Context, MediaError, stream_fps};

pub(crate) const PLANAR_F32: Sample = Sample::F32(sample::Type::Planar);

/// The hardware decoders to try, best first: libav's device type name and
/// the name shown in the status line.
#[cfg(target_os = "linux")]
const HW_DECODERS: &[(&str, &str)] = &[("cuda", "NVDEC"), ("vaapi", "VAAPI")];
#[cfg(target_os = "windows")]
const HW_DECODERS: &[(&str, &str)] = &[("d3d11va", "D3D11VA")];
#[cfg(target_os = "macos")]
const HW_DECODERS: &[(&str, &str)] = &[("videotoolbox", "VideoToolbox")];
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
const HW_DECODERS: &[(&str, &str)] = &[];

/// Whether `MONTAGE_HWDEC=0` (or `software`, `no`, `off`) asks for software
/// decoding, to rule the GPU decoder out when something looks wrong.
fn hw_disabled() -> bool {
    std::env::var("MONTAGE_HWDEC").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "no" | "off" | "false" | "software" | "sw"
        )
    })
}

/// The start time of the file, in seconds: every timestamp Montage shows is
/// relative to it.
pub(crate) fn start_offset(ictx: &ffmpeg::format::context::Input) -> f64 {
    // SAFETY: reads a plain field of the open context.
    let start = unsafe { (*ictx.as_ptr()).start_time };
    if start == ffi::AV_NOPTS_VALUE {
        0.0
    } else {
        start as f64 / f64::from(ffi::AV_TIME_BASE)
    }
}

/// Tells the demuxer to skip every stream but `keep`, so reading the video
/// doesn't read the audio and the other way round.
pub(crate) fn discard_others(ictx: &mut ffmpeg::format::context::Input, keep: &[usize]) {
    // SAFETY: `streams` holds `nb_streams` valid stream pointers while the
    // context is open; `discard` is a plain field the caller may set.
    unsafe {
        let ctx = ictx.as_mut_ptr();
        for i in 0..(*ctx).nb_streams as usize {
            if !keep.contains(&i) {
                (**(*ctx).streams.add(i)).discard = ffi::AVDiscard::AVDISCARD_ALL;
            }
        }
    }
}

/// Seeks the demuxer to the keyframe at or before `seconds` (file time, i.e.
/// with the start offset added), or to the first one when there's none
/// before.
pub(crate) fn seek_before(ictx: &mut ffmpeg::format::context::Input, seconds: f64) {
    let ts = (seconds * f64::from(ffi::AV_TIME_BASE)) as i64;
    if ictx.seek(ts, ..ts).is_err() {
        let _ = ictx.seek(ts, ..);
    }
}

/// Reads the next packet, skipping corrupt ones. `None` at the end of the
/// file or on a read error.
pub(crate) fn read_packet(ictx: &mut ffmpeg::format::context::Input) -> Option<Packet> {
    loop {
        let mut packet = Packet::empty();
        match packet.read(ictx) {
            Ok(()) => return Some(packet),
            Err(ffmpeg::Error::InvalidData) => continue,
            Err(_) => return None,
        }
    }
}

/// Fits `width` x `height` within `max_width` x `max_height`, keeping the
/// aspect ratio, never enlarging.
pub(crate) fn fit(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (max_width.max(1), max_height.max(1));
    }
    let scale = (f64::from(max_width.max(1)) / f64::from(width))
        .min(f64::from(max_height.max(1)) / f64::from(height))
        .min(1.0);
    let w = (f64::from(width) * scale).round().max(1.0) as u32;
    let h = (f64::from(height) * scale).round().max(1.0) as u32;
    (w, h)
}

/// How the software decoder may use threads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Threads {
    /// Frame and slice threads on every core: best throughput, for playback.
    Auto,
    /// One thread: lowest latency per frame, for the thumbnails, which run
    /// several decoders side by side.
    One,
}

/// A decoded frame.
pub(crate) struct Decoded {
    /// Seconds from the start of the clip.
    pub pts: f64,
    /// In memory (downloaded from the GPU when needed); `None` when the
    /// caller didn't want the pixels.
    pub frame: Option<frame::Video>,
}

/// The clip's video stream and its decoder.
pub(crate) struct VideoSource {
    ictx: ffmpeg::format::context::Input,
    index: usize,
    tb: Rational,
    offset: f64,
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    params: codec::Parameters,
    codec_name: String,
    threads: Threads,
    decoder: ffmpeg::decoder::Video,
    /// The hardware decoder in use, by its status line name.
    hw: Option<&'static str>,
    eof_sent: bool,
    /// Where to restart after falling back to software: the last seek.
    resume_at: f64,
    /// Frames up to this pts were already returned before a fallback.
    drop_through: Option<f64>,
    last_pts: Option<f64>,
    decoded: frame::Video,
}

impl VideoSource {
    /// Opens the video of `path`, on the GPU when `hardware` and the system
    /// has a decoder for the codec.
    pub fn open(path: &Path, hardware: bool, threads: Threads) -> Result<VideoSource, MediaError> {
        crate::init();
        let mut ictx =
            ffmpeg::format::input(path).context(|| format!("can't open {}", path.display()))?;
        let stream = ictx
            .streams()
            .best(media::Type::Video)
            .ok_or_else(|| MediaError::NoVideo(path.into()))?;
        let index = stream.index();
        let tb = stream.time_base();
        let fps = stream_fps(&stream);
        let params = stream.parameters();
        let codec_name = params.id().name().to_string();
        discard_others(&mut ictx, &[index]);
        let offset = start_offset(&ictx);

        let mut hw = None;
        let mut decoder = None;
        if hardware && !hw_disabled() {
            for &(device, label) in HW_DECODERS {
                if let Some(d) = open_hw_decoder(&params, tb, device) {
                    decoder = Some(d);
                    hw = Some(label);
                    break;
                }
            }
        }
        let decoder = match decoder {
            Some(d) => d,
            None => open_sw_decoder(&params, tb, threads)
                .context(|| format!("can't decode the video of {}", path.display()))?,
        };
        Ok(VideoSource {
            ictx,
            index,
            tb,
            offset,
            fps: if fps > 0.0 { fps } else { 30.0 },
            width: decoder.width(),
            height: decoder.height(),
            params,
            codec_name,
            threads,
            decoder,
            hw,
            eof_sent: false,
            resume_at: 0.0,
            drop_through: None,
            last_pts: None,
            decoded: frame::Video::empty(),
        })
    }

    /// The hardware decoder in use, by its status line name.
    pub fn hw(&self) -> Option<&'static str> {
        self.hw
    }

    /// `hevc (VideoToolbox)`, `h264 (software)`…
    pub fn decoder_name(&self) -> String {
        format!("{} ({})", self.codec_name, self.hw.unwrap_or("software"))
    }

    /// Seeks to the keyframe at or before `t` seconds. The next frames are
    /// the ones from there.
    pub fn seek(&mut self, t: f64) {
        seek_before(&mut self.ictx, t.max(0.0) + self.offset);
        self.decoder.flush();
        self.eof_sent = false;
        self.resume_at = t;
        self.drop_through = None;
        self.last_pts = None;
    }

    /// Reads the next packet of the video stream, `None` at the end.
    pub fn next_packet(&mut self) -> Option<Packet> {
        loop {
            let packet = read_packet(&mut self.ictx)?;
            if packet.stream() == self.index {
                return Some(packet);
            }
        }
    }

    /// The packet's pts in seconds from the start of the clip.
    pub fn packet_pts(&self, packet: &Packet) -> Option<f64> {
        packet
            .pts()
            .or(packet.dts())
            .map(|ts| ts as f64 * f64::from(self.tb) - self.offset)
    }

    /// Decodes `packet` alone (the decoder is flushed before and drained
    /// after), for the thumbnails' keyframes.
    pub fn decode_alone(&mut self, packet: &Packet) -> Result<Option<frame::Video>, MediaError> {
        self.decoder.flush();
        self.decoder
            .send_packet(packet)
            .context(|| "can't decode the video".into())?;
        let _ = self.decoder.send_eof();
        let mut out = None;
        let mut f = frame::Video::empty();
        while self.decoder.receive_frame(&mut f).is_ok() {
            if out.is_none() {
                out = Some(std::mem::replace(&mut f, frame::Video::empty()));
            }
        }
        self.decoder.flush();
        self.eof_sent = false;
        match out {
            Some(f) => self.download(f).map(Some),
            None => Ok(None),
        }
    }

    /// The next frame in display order, `None` at the end of the stream.
    /// The frame comes only when `want_pixels(pts)`, copied from the GPU
    /// when `to_memory` (else maybe still a hardware frame, see
    /// [`download`]).
    pub fn next_frame(
        &mut self,
        want_pixels: impl Fn(f64) -> bool,
        to_memory: bool,
    ) -> Result<Option<Decoded>, MediaError> {
        loop {
            match self.decoder.receive_frame(&mut self.decoded) {
                Ok(()) => {
                    let is_hw = is_hw_frame(&self.decoded);
                    if self.hw.is_some() && !is_hw {
                        // libav picked software itself (a profile the GPU
                        // can't do): decode in software with every thread.
                        self.fall_back()?;
                        continue;
                    }
                    let pts = match self.decoded.timestamp() {
                        Some(ts) => ts as f64 * f64::from(self.tb) - self.offset,
                        None => self.last_pts.map_or(0.0, |p| p + 1.0 / self.fps),
                    };
                    if self.drop_through.is_some_and(|d| pts <= d + 1e-6) {
                        continue;
                    }
                    self.drop_through = None;
                    let frame = std::mem::replace(&mut self.decoded, frame::Video::empty());
                    let frame = if want_pixels(pts) && !to_memory {
                        Some(frame)
                    } else if want_pixels(pts) {
                        match self.download(frame) {
                            Ok(f) => Some(f),
                            Err(_) if self.hw.is_some() => {
                                self.fall_back()?;
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    } else {
                        None
                    };
                    self.last_pts = Some(pts);
                    return Ok(Some(Decoded { pts, frame }));
                }
                Err(ffmpeg::Error::Other { errno: ffi::EAGAIN }) => {
                    if self.eof_sent {
                        return Ok(None);
                    }
                    match self.next_packet() {
                        Some(packet) => match self.decoder.send_packet(&packet) {
                            Ok(()) => {}
                            Err(_) if self.hw.is_some() => self.fall_back()?,
                            // A corrupt packet: skip it.
                            Err(_) => {}
                        },
                        None => {
                            let _ = self.decoder.send_eof();
                            self.eof_sent = true;
                        }
                    }
                }
                Err(ffmpeg::Error::Eof) => return Ok(None),
                Err(_) if self.hw.is_some() => self.fall_back()?,
                Err(e) => return Err(e).context(|| "can't decode the video".into()),
            }
        }
    }

    /// Reopens the decoder in software and goes back to where it was.
    fn fall_back(&mut self) -> Result<(), MediaError> {
        self.decoder = open_sw_decoder(&self.params, self.tb, self.threads)
            .context(|| "can't decode the video in software".into())?;
        self.hw = None;
        let done = self.last_pts;
        self.seek(self.resume_at);
        self.drop_through = done;
        self.last_pts = done;
        Ok(())
    }

    /// The frame in memory: copied from the GPU when it's a hardware frame.
    fn download(&self, f: frame::Video) -> Result<frame::Video, MediaError> {
        download(f, self.maps_frames())
    }

    /// Whether hardware frames are better mapped than copied: with
    /// VideoToolbox, they're in memory the CPU shares.
    pub fn maps_frames(&self) -> bool {
        self.hw == Some("VideoToolbox")
    }

    /// Switches to software decoding from where it is, after a hardware
    /// frame couldn't be read.
    pub fn force_software(&mut self) -> Result<(), MediaError> {
        if self.hw.is_some() {
            self.fall_back()?;
        }
        Ok(())
    }
}

/// The frame in memory: a hardware frame is mapped (`map`) or copied from
/// the GPU. Any thread may call it.
pub(crate) fn download(f: frame::Video, map: bool) -> Result<frame::Video, MediaError> {
    if !is_hw_frame(&f) {
        return Ok(f);
    }
    let mut sw = frame::Video::empty();
    // SAFETY: `f` is a valid hardware frame (it has a frames context);
    // `sw` is a clean frame the calls fill. A mapped frame keeps a
    // reference to `f`'s buffer and unmaps it when freed.
    let ret = unsafe {
        let mut ret = -1;
        if map {
            ret = ffi::av_hwframe_map(sw.as_mut_ptr(), f.as_ptr(), ffi::AV_HWFRAME_MAP_READ as i32);
        }
        if ret < 0 {
            ret = ffi::av_hwframe_transfer_data(sw.as_mut_ptr(), f.as_ptr(), 0);
        }
        if ret >= 0 {
            ffi::av_frame_copy_props(sw.as_mut_ptr(), f.as_ptr());
        }
        ret
    };
    if ret < 0 {
        return Err(MediaError::Ffmpeg {
            context: "can't copy a frame from the GPU".into(),
            source: ffmpeg::Error::from(ret),
        });
    }
    Ok(sw)
}

fn is_hw_frame(f: &frame::Video) -> bool {
    // SAFETY: reads a plain field of a valid frame.
    unsafe { !(*f.as_ptr()).hw_frames_ctx.is_null() }
}

fn open_sw_decoder(
    params: &codec::Parameters,
    tb: Rational,
    threads: Threads,
) -> Result<ffmpeg::decoder::Video, ffmpeg::Error> {
    let mut ctx = codec::context::Context::from_parameters(params.clone())?;
    ctx.set_threading(match threads {
        Threads::Auto => threading::Config {
            kind: threading::Type::Frame,
            count: 0,
        },
        Threads::One => threading::Config {
            kind: threading::Type::Slice,
            count: 1,
        },
    });
    let mut decoder = ctx.decoder();
    decoder.set_packet_time_base(tb);
    decoder.video()
}

/// Picks the hardware pixel format stored in the context's `opaque`, or
/// lets libav choose (software) when the decoder doesn't offer it.
unsafe extern "C" fn get_hw_format(
    ctx: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    // SAFETY: libav passes its context and a list ended by NONE; `opaque`
    // holds the format set in `open_hw_decoder`.
    unsafe {
        let wanted = (*ctx).opaque as isize as i32;
        let mut p = formats;
        while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p as i32 == wanted {
                return *p;
            }
            p = p.add(1);
        }
        ffi::avcodec_default_get_format(ctx, formats)
    }
}

/// Opens the decoder on the `device` hardware decoder (`cuda`, `vaapi`,
/// `d3d11va`, `videotoolbox`), or `None` when libav, the system or the codec
/// lacks it.
fn open_hw_decoder(
    params: &codec::Parameters,
    tb: Rational,
    device: &str,
) -> Option<ffmpeg::decoder::Video> {
    let codec = ffmpeg::decoder::find(params.id())?;
    let name = CString::new(device).ok()?;
    // SAFETY: plain libav calls on valid pointers; the device reference is
    // handed to the codec context, which unrefs it when freed, or unreffed
    // here on failure.
    unsafe {
        let kind = ffi::av_hwdevice_find_type_by_name(name.as_ptr());
        if kind == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
            return None;
        }
        let mut pix_fmt = None;
        for i in 0.. {
            let config = ffi::avcodec_get_hw_config(codec.as_ptr(), i);
            if config.is_null() {
                break;
            }
            let method = ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32;
            if (*config).methods & method != 0 && (*config).device_type == kind {
                pix_fmt = Some((*config).pix_fmt);
                break;
            }
        }
        let pix_fmt = pix_fmt?;

        let mut device_ref: *mut ffi::AVBufferRef = ptr::null_mut();
        if ffi::av_hwdevice_ctx_create(&mut device_ref, kind, ptr::null(), ptr::null_mut(), 0) < 0
            || device_ref.is_null()
        {
            return None;
        }
        let Ok(mut ctx) = codec::context::Context::from_parameters(params.clone()) else {
            ffi::av_buffer_unref(&mut device_ref);
            return None;
        };
        let raw = ctx.as_mut_ptr();
        (*raw).hw_device_ctx = device_ref;
        (*raw).opaque = pix_fmt as i32 as isize as *mut c_void;
        (*raw).get_format = Some(get_hw_format);
        // The player holds a few frames between decoding and copying them;
        // fixed size pools (CUDA, VAAPI, D3D11) need room for them.
        (*raw).extra_hw_frames = 4;
        ctx.set_threading(threading::Config {
            kind: threading::Type::Slice,
            count: 1,
        });
        let mut decoder = ctx.decoder();
        decoder.set_packet_time_base(tb);
        decoder.video().ok()
    }
}

/// Converts decoded frames to RGBA at a given size, keeping the scaler
/// between frames.
pub(crate) struct ToRgba {
    flags: scaling::Flags,
    scaler: Option<(scaling::Context, ScaleKey)>,
}

/// What a scaler was made for: the input format and size, the output size.
type ScaleKey = (Pixel, u32, u32, u32, u32);

/// Takes decoded audio: the frame's start in seconds of file time (if
/// known), the rate, the left and right samples.
pub(crate) type AudioSink<'a> = dyn FnMut(Option<f64>, u32, &[f32], &[f32]) + 'a;

impl ToRgba {
    pub fn new(flags: scaling::Flags) -> ToRgba {
        ToRgba {
            flags,
            scaler: None,
        }
    }

    pub fn convert(
        &mut self,
        f: &frame::Video,
        width: u32,
        height: u32,
    ) -> Result<RgbaImage, MediaError> {
        let key = (f.format(), f.width(), f.height(), width, height);
        if self.scaler.as_ref().is_none_or(|(_, k)| *k != key) {
            let mut scaler = scaling::Context::get(
                f.format(),
                f.width(),
                f.height(),
                Pixel::RGBA,
                width,
                height,
                self.flags,
            )
            .context(|| "can't set up scaling".into())?;
            set_colorspace(&mut scaler, f);
            self.scaler = Some((scaler, key));
        }
        let (scaler, _) = self.scaler.as_mut().expect("set above");
        let mut out = frame::Video::new(Pixel::RGBA, width, height);
        scaler
            .run(f, &mut out)
            .context(|| "can't scale a frame".into())?;
        let row = width as usize * 4;
        let stride = out.stride(0);
        let data = out.data(0);
        let mut image = RgbaImage {
            width,
            height,
            data: Vec::with_capacity(row * height as usize),
        };
        for y in 0..height as usize {
            image
                .data
                .extend_from_slice(&data[y * stride..y * stride + row]);
        }
        Ok(image)
    }
}

/// Tells swscale the frame's YUV matrix and range (it assumes BT.601,
/// limited range, which tints HD video).
fn set_colorspace(scaler: &mut scaling::Context, f: &frame::Video) {
    // SAFETY: reads plain fields of a valid frame and sets the details of a
    // valid scaler; the coefficient tables are static in libswscale.
    unsafe {
        let raw = f.as_ptr();
        if !is_yuv(f.format()) {
            return;
        }
        let space = match (*raw).colorspace {
            ffi::AVColorSpace::AVCOL_SPC_BT709 => ffi::SWS_CS_ITU709,
            ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL | ffi::AVColorSpace::AVCOL_SPC_BT2020_CL => {
                ffi::SWS_CS_BT2020
            }
            ffi::AVColorSpace::AVCOL_SPC_UNSPECIFIED if f.height() >= 720 => ffi::SWS_CS_ITU709,
            _ => ffi::SWS_CS_DEFAULT,
        };
        let full = i32::from((*raw).color_range == ffi::AVColorRange::AVCOL_RANGE_JPEG);
        ffi::sws_setColorspaceDetails(
            scaler.as_mut_ptr(),
            ffi::sws_getCoefficients(space),
            full,
            ffi::sws_getCoefficients(ffi::SWS_CS_DEFAULT),
            1,
            0,
            1 << 16,
            1 << 16,
        );
    }
}

fn is_yuv(format: Pixel) -> bool {
    !matches!(
        format,
        Pixel::RGB24
            | Pixel::BGR24
            | Pixel::RGBA
            | Pixel::BGRA
            | Pixel::ARGB
            | Pixel::ABGR
            | Pixel::RGBZ
            | Pixel::BGRZ
            | Pixel::GRAY8
    )
}

/// One audio track's decoder and its resampler to stereo f32.
pub(crate) struct AudioTrack {
    pub stream: usize,
    pub tb: Rational,
    pub decoder: ffmpeg::decoder::Audio,
    rate: u32,
    resampler: Option<resampling::Context>,
}

impl AudioTrack {
    /// Every audio track of the file, resampled to `rate` (0 keeps each
    /// track's own rate).
    pub fn open_all(
        ictx: &ffmpeg::format::context::Input,
        rate: u32,
    ) -> Result<Vec<AudioTrack>, MediaError> {
        let mut tracks = Vec::new();
        let audio = ictx
            .streams()
            .filter(|s| s.parameters().medium() == media::Type::Audio);
        for (n, stream) in audio.enumerate() {
            let mut ctx = codec::context::Context::from_parameters(stream.parameters())
                .context(|| format!("can't read audio track {}", n + 1))?;
            ctx.set_threading(threading::Config {
                kind: threading::Type::Slice,
                count: 1,
            });
            let mut decoder = ctx.decoder();
            decoder.set_packet_time_base(stream.time_base());
            let decoder = decoder
                .audio()
                .context(|| format!("can't open audio track {}", n + 1))?;
            tracks.push(AudioTrack {
                stream: stream.index(),
                tb: stream.time_base(),
                decoder,
                rate,
                resampler: None,
            });
        }
        Ok(tracks)
    }

    /// Decodes what's pending and hands `out` each frame: its start in
    /// seconds of file time (if known), the rate, and the left and right
    /// samples.
    pub fn drain(&mut self, out: &mut AudioSink) -> Result<(), MediaError> {
        let mut decoded = frame::Audio::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            let in_rate = decoded.rate().max(1);
            let out_rate = if self.rate == 0 { in_rate } else { self.rate };
            let fits = self.resampler.as_ref().is_some_and(|r| {
                r.input().format == decoded.format()
                    && r.input().rate == in_rate
                    && r.input().channel_layout.channels() == decoded.channel_layout().channels()
                    && r.output().rate == out_rate
            });
            if !fits {
                let mut layout = decoded.channel_layout();
                if layout.0.order == ffi::AVChannelOrder::AV_CHANNEL_ORDER_UNSPEC {
                    layout = ChannelLayout::default(layout.channels());
                }
                self.resampler = Some(
                    resampling::Context::get(
                        decoded.format(),
                        layout,
                        in_rate,
                        PLANAR_F32,
                        ChannelLayout::STEREO,
                        out_rate,
                    )
                    .context(|| "can't set up audio resampling".into())?,
                );
            }
            let resampler = self.resampler.as_mut().expect("set above");
            let room = decoded.samples() as u64 * u64::from(out_rate) / u64::from(in_rate) + 256;
            let mut resampled = frame::Audio::new(PLANAR_F32, room as usize, ChannelLayout::STEREO);
            resampler
                .run(&decoded, &mut resampled)
                .context(|| "can't resample the audio".into())?;
            let n = resampled.samples();
            let at = decoded.timestamp().map(|ts| ts as f64 * f64::from(self.tb));
            out(
                at,
                out_rate,
                &resampled.plane::<f32>(0)[..n],
                &resampled.plane::<f32>(1)[..n],
            );
        }
        Ok(())
    }

    /// Forgets the decoder's and the resampler's state, after a seek.
    pub fn flush(&mut self) {
        self.decoder.flush();
        self.resampler = None;
    }
}
