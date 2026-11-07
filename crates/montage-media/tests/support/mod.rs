//! Clips made at test time with libav, and ways to read exports back.
//!
//! Each video frame carries its own index as a bar code: the top quarter is
//! split into `BITS` blocks, white for a 1 bit and black for a 0, most
//! significant first. The bars survive heavy compression, so a test can tell
//! which source frame an exported frame came from.

#![allow(dead_code)]

use std::path::Path;

use ffmpeg_next as ffmpeg;
use ffmpeg_next::format::{Pixel, Sample, sample};
use ffmpeg_next::{ChannelLayout, Dictionary, Packet, codec, encoder, frame, media};

pub const BITS: u32 = 9;
const RATE: i32 = 48_000;

pub struct Spec {
    pub width: u32,
    pub height: u32,
    pub fps: i32,
    pub frames: i64,
    /// One audio track per entry: a sine tone's frequency and amplitude.
    pub tones: Vec<(f32, f32)>,
    /// Amplitude of the noise in the bottom half, which no encoder can
    /// predict: it keeps x264 short of bits, so the size tests check real
    /// rate control. 6 is reachable at the tests' bitrates; 40 makes x264
    /// overshoot.
    pub noise: u32,
}

impl Spec {
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / f64::from(self.fps)
    }
}

pub fn make_clip(path: &Path, spec: &Spec) {
    ffmpeg::init().unwrap();
    let mut octx = ffmpeg::format::output_as(path, "mp4").unwrap();
    let global = octx
        .format()
        .flags()
        .contains(ffmpeg::format::Flags::GLOBAL_HEADER);

    let vcodec = encoder::find_by_name("libx264").unwrap();
    let mut venc = codec::context::Context::new_with_codec(vcodec)
        .encoder()
        .video()
        .unwrap();
    venc.set_width(spec.width);
    venc.set_height(spec.height);
    venc.set_format(Pixel::YUV420P);
    venc.set_time_base((1, spec.fps));
    venc.set_frame_rate(Some((spec.fps, 1)));
    // Frequent keyframes, so seeking lands well before the range start
    // and the accurate trim is exercised.
    venc.set_gop(15);
    if global {
        venc.set_flags(codec::Flags::GLOBAL_HEADER);
    }
    let mut opts = Dictionary::new();
    opts.set("preset", "ultrafast");
    opts.set("crf", "20");
    let mut venc = venc.open_as_with(vcodec, opts).unwrap();
    let v_index = {
        let mut ost = octx.add_stream(vcodec).unwrap();
        ost.set_parameters(&venc);
        ost.set_time_base((1, spec.fps));
        ost.index()
    };

    let acodec = encoder::find_by_name("aac").unwrap();
    let mut aencs = Vec::new();
    for _ in &spec.tones {
        let mut aenc = codec::context::Context::new_with_codec(acodec)
            .encoder()
            .audio()
            .unwrap();
        aenc.set_rate(RATE);
        aenc.set_channel_layout(ChannelLayout::STEREO);
        aenc.set_format(Sample::F32(sample::Type::Planar));
        aenc.set_bit_rate(128_000);
        aenc.set_time_base((1, RATE));
        if global {
            aenc.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        let aenc = aenc.open_as(acodec).unwrap();
        let index = {
            let mut ost = octx.add_stream(acodec).unwrap();
            ost.set_parameters(&aenc);
            ost.set_time_base((1, RATE));
            ost.index()
        };
        aencs.push((aenc, index));
    }

    octx.write_header().unwrap();

    let v_tb = octx.stream(v_index).unwrap().time_base();
    let write_video = |venc: &mut encoder::video::Encoder, octx: &mut _| {
        let mut packet = Packet::empty();
        while venc.receive_packet(&mut packet).is_ok() {
            packet.set_stream(v_index);
            packet.set_duration(1);
            packet.rescale_ts((1, spec.fps), v_tb);
            packet.write_interleaved(octx).unwrap();
        }
    };
    for i in 0..spec.frames {
        let mut f = draw(spec.width, spec.height, i as u32, spec.noise.max(1));
        f.set_pts(Some(i));
        venc.send_frame(&f).unwrap();
        write_video(&mut venc, &mut octx);
    }
    venc.send_eof().unwrap();
    write_video(&mut venc, &mut octx);

    let samples = (spec.seconds() * f64::from(RATE)).round() as usize;
    for ((freq, amp), (aenc, index)) in spec.tones.iter().zip(aencs.iter_mut()) {
        let a_tb = octx.stream(*index).unwrap().time_base();
        let frame_size = aenc.frame_size() as usize;
        let write_audio = |aenc: &mut encoder::audio::Encoder, octx: &mut _| {
            let mut packet = Packet::empty();
            while aenc.receive_packet(&mut packet).is_ok() {
                packet.set_stream(*index);
                packet.rescale_ts((1, RATE), a_tb);
                packet.write_interleaved(octx).unwrap();
            }
        };
        let mut pos = 0;
        while pos < samples {
            let mut f = frame::Audio::new(
                Sample::F32(sample::Type::Planar),
                frame_size,
                ChannelLayout::STEREO,
            );
            f.set_rate(RATE as u32);
            for ch in 0..2 {
                for (i, s) in f.plane_mut::<f32>(ch).iter_mut().enumerate() {
                    let t = (pos + i) as f32 / RATE as f32;
                    *s = amp * (2.0 * std::f32::consts::PI * freq * t).sin();
                }
            }
            f.set_pts(Some(pos as i64));
            aenc.send_frame(&f).unwrap();
            write_audio(aenc, &mut octx);
            pos += frame_size;
        }
        aenc.send_eof().unwrap();
        write_audio(aenc, &mut octx);
    }

    octx.write_trailer().unwrap();
}

/// A frame with the bar code of `index` on top and moving stripes below.
fn draw(width: u32, height: u32, index: u32, amplitude: u32) -> frame::Video {
    let mut f = frame::Video::new(Pixel::YUV420P, width, height);
    let band = height / 4;
    let block = width / BITS;
    let stride = f.stride(0);
    let y = f.data_mut(0);
    for row in 0..height {
        for col in 0..width {
            let v = if row < band {
                let bit = (col / block).min(BITS - 1);
                if index >> (BITS - 1 - bit) & 1 == 1 {
                    235
                } else {
                    16
                }
            } else if row >= height / 2 {
                noise(col, row, index, amplitude)
            } else if ((col + 4 * index) / 16).is_multiple_of(2) {
                60
            } else {
                170
            };
            y[row as usize * stride + col as usize] = v;
        }
    }
    for plane in 1..3 {
        f.data_mut(plane).fill(128);
    }
    f
}

fn noise(x: u32, y: u32, i: u32, amplitude: u32) -> u8 {
    let mut h =
        x.wrapping_mul(0x9e37_79b1) ^ y.wrapping_mul(0x85eb_ca77) ^ i.wrapping_mul(0xc2b2_ae3d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    (100 + h % amplitude) as u8
}

/// Reads the bar code of every frame of the clip, in display order.
pub fn frame_indices(path: &Path) -> Vec<u32> {
    let mut ictx = ffmpeg::format::input(path).unwrap();
    let stream = ictx.streams().best(media::Type::Video).unwrap();
    let index = stream.index();
    let mut decoder = codec::context::Context::from_parameters(stream.parameters())
        .unwrap()
        .decoder()
        .video()
        .unwrap();
    let mut out = Vec::new();
    let mut f = frame::Video::empty();
    let mut read = |decoder: &mut ffmpeg::decoder::Video, out: &mut Vec<u32>| {
        while decoder.receive_frame(&mut f).is_ok() {
            out.push(read_bars(&f));
        }
    };
    for (s, packet) in ictx.packets() {
        if s.index() == index {
            decoder.send_packet(&packet).unwrap();
            read(&mut decoder, &mut out);
        }
    }
    decoder.send_eof().unwrap();
    read(&mut decoder, &mut out);
    out
}

fn read_bars(f: &frame::Video) -> u32 {
    let (w, h) = (f.width(), f.height());
    let block = w / BITS;
    let stride = f.stride(0);
    let y = f.data(0);
    let mut value = 0;
    for bit in 0..BITS {
        // The middle half of the block, away from blurred edges.
        let (x0, x1) = (bit * block + block / 4, bit * block + 3 * block / 4);
        let (y0, y1) = (h / 16, 3 * h / 16);
        let mut sum = 0u64;
        let mut n = 0u64;
        for row in y0..y1 {
            for col in x0..x1 {
                sum += u64::from(y[row as usize * stride + col as usize]);
                n += 1;
            }
        }
        value = value << 1 | u32::from(sum / n > 128);
    }
    value
}

/// The first channel of the clip's first audio track, decoded.
pub fn audio_samples(path: &Path) -> (Vec<f32>, u32) {
    let mut ictx = ffmpeg::format::input(path).unwrap();
    let stream = ictx.streams().best(media::Type::Audio).unwrap();
    let index = stream.index();
    let mut decoder = codec::context::Context::from_parameters(stream.parameters())
        .unwrap()
        .decoder()
        .audio()
        .unwrap();
    let mut out = Vec::new();
    let mut rate = 0;
    let mut f = frame::Audio::empty();
    let mut read = |decoder: &mut ffmpeg::decoder::Audio, out: &mut Vec<f32>| {
        while decoder.receive_frame(&mut f).is_ok() {
            assert_eq!(f.format(), Sample::F32(sample::Type::Planar));
            rate = f.rate();
            out.extend_from_slice(&f.plane::<f32>(0)[..f.samples()]);
        }
    };
    for (s, packet) in ictx.packets() {
        if s.index() == index {
            decoder.send_packet(&packet).unwrap();
            read(&mut decoder, &mut out);
        }
    }
    decoder.send_eof().unwrap();
    read(&mut decoder, &mut out);
    (out, rate)
}

/// The amplitude of the sine at `freq` in `samples` (Goertzel).
pub fn tone_amplitude(samples: &[f32], rate: u32, freq: f32) -> f32 {
    let w = 2.0 * std::f64::consts::PI * f64::from(freq) / f64::from(rate);
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in samples {
        let s = f64::from(x) + coeff * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    (2.0 * power.sqrt() / samples.len() as f64) as f32
}

/// A decoded frame as straight RGBA, rows packed.
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Rgba {
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        [
            self.data[i],
            self.data[i + 1],
            self.data[i + 2],
            self.data[i + 3],
        ]
    }
}

/// Every frame of the clip's video (MP4 or GIF), in display order, as RGBA.
pub fn rgba_frames(path: &Path) -> Vec<Rgba> {
    use ffmpeg_next::software::scaling;

    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(path).unwrap();
    let stream = ictx.streams().best(media::Type::Video).unwrap();
    let index = stream.index();
    let mut decoder = codec::context::Context::from_parameters(stream.parameters())
        .unwrap()
        .decoder()
        .video()
        .unwrap();
    let mut out = Vec::new();
    let mut f = frame::Video::empty();
    let mut read = |decoder: &mut ffmpeg::decoder::Video, out: &mut Vec<Rgba>| {
        while decoder.receive_frame(&mut f).is_ok() {
            let (w, h) = (f.width(), f.height());
            let mut scaler =
                scaling::Context::get(f.format(), w, h, Pixel::RGBA, w, h, scaling::Flags::POINT)
                    .unwrap();
            let mut rgba = frame::Video::new(Pixel::RGBA, w, h);
            scaler.run(&f, &mut rgba).unwrap();
            let stride = rgba.stride(0);
            let mut data = Vec::with_capacity(w as usize * h as usize * 4);
            for row in 0..h as usize {
                data.extend_from_slice(&rgba.data(0)[row * stride..row * stride + w as usize * 4]);
            }
            out.push(Rgba {
                width: w,
                height: h,
                data,
            });
        }
    };
    for (s, packet) in ictx.packets() {
        if s.index() == index {
            decoder.send_packet(&packet).unwrap();
            read(&mut decoder, &mut out);
        }
    }
    decoder.send_eof().unwrap();
    read(&mut decoder, &mut out);
    out
}

/// A video packet as stored: time and duration in seconds, keyframe flag,
/// size in bytes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PacketInfo {
    pub pts: f64,
    pub duration: f64,
    pub key: bool,
    pub size: usize,
}

/// The packets of the clip's video stream, in file order.
pub fn video_packets(path: &Path) -> Vec<PacketInfo> {
    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(path).unwrap();
    let stream = ictx.streams().best(media::Type::Video).unwrap();
    let (index, tb) = (stream.index(), f64::from(stream.time_base()));
    ictx.packets()
        .filter(|(s, _)| s.index() == index)
        .map(|(_, p)| PacketInfo {
            pts: p.pts().or(p.dts()).unwrap_or(0) as f64 * tb,
            duration: p.duration() as f64 * tb,
            key: p.is_key(),
            size: p.size(),
        })
        .collect()
}

/// How many audio streams the file has.
pub fn audio_stream_count(path: &Path) -> usize {
    ffmpeg::init().unwrap();
    let ictx = ffmpeg::format::input(path).unwrap();
    ictx.streams()
        .filter(|s| s.parameters().medium() == media::Type::Audio)
        .count()
}
