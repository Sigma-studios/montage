//! Decoding, encoding and muxing through libav (FFmpeg's libraries, linked in
//! through `ffmpeg-next`; the ffmpeg program is never run).
//!
//! For now: probing a clip, and the Share export (H.264 two-pass + AAC under
//! a size budget). Decoding is in software; GPU decoding comes after the
//! phase 0 spike.

mod composite;
mod decode;
mod encoders;
mod export;
mod full;
mod gif;
mod job;
mod player;
mod thumbs;

use std::path::{Path, PathBuf};
use std::sync::Once;

use ffmpeg_next as ffmpeg;
use ffmpeg_next::media;

pub use export::{export_share, export_share_within};
pub use job::{ExportJob, ExportPreset, ExportReport, estimate_bytes, export};
pub use player::{Player, VideoFrame};
pub use thumbs::{Waveform, filmstrip, thumbnail, waveform};

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("{context}: {source}")]
    Ffmpeg {
        context: String,
        source: ffmpeg::Error,
    },
    #[error("{0} has no video stream")]
    NoVideo(PathBuf),
    #[error("this FFmpeg has no {0} encoder")]
    NoEncoder(&'static str),
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("nothing to export: the range holds no video frames")]
    EmptyRange,
    #[error("the export is {size} bytes, over the budget of {max_bytes}, even at a lower bitrate")]
    OverBudget { size: u64, max_bytes: u64 },
    #[error("export cancelled")]
    Cancelled,
    #[error("no audio output: {0}")]
    Audio(String),
    #[error("GIF encoding failed: {0}")]
    Gif(String),
}

/// Wraps an FFmpeg error with what was being done.
pub(crate) trait Context<T> {
    fn context(self, what: impl FnOnce() -> String) -> Result<T, MediaError>;
}

impl<T> Context<T> for Result<T, ffmpeg::Error> {
    fn context(self, what: impl FnOnce() -> String) -> Result<T, MediaError> {
        self.map_err(|source| MediaError::Ffmpeg {
            context: what(),
            source,
        })
    }
}

/// Initialises libav once, quietly: warnings would land in the terminal of
/// every `montage share`.
pub(crate) fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Only fails on FFmpeg builds older than anything ffmpeg-next 9
        // supports.
        let _ = ffmpeg::init();
        ffmpeg::log::set_level(ffmpeg::log::Level::Error);
    });
}

/// What Montage needs to know about a clip.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    /// Seconds.
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// FFmpeg's name for the video codec: `hevc`, `h264`, `av1`…
    pub video_codec: String,
    pub audio_tracks: usize,
    /// The whole file's bitrate in bit/s, from the container (or its size
    /// over its duration when the container doesn't say); 0 when unknown.
    pub bit_rate: u64,
}

impl MediaInfo {
    pub fn source(&self) -> montage_core::share::Source {
        montage_core::share::Source {
            duration: self.duration,
            width: self.width,
            height: self.height,
            fps: self.fps,
        }
    }
}

/// Reads a clip's duration, size, frame rate and tracks.
pub fn probe(path: &Path) -> Result<MediaInfo, MediaError> {
    init();
    let ictx = ffmpeg::format::input(path).context(|| format!("can't open {}", path.display()))?;
    let video = ictx
        .streams()
        .best(media::Type::Video)
        .ok_or_else(|| MediaError::NoVideo(path.into()))?;

    let params = video.parameters();
    let video_codec = params.id().name().to_string();
    let decoder = ffmpeg::codec::context::Context::from_parameters(params)
        .and_then(|c| c.decoder().video())
        .context(|| format!("can't read the video of {}", path.display()))?;

    let fps = stream_fps(&video);
    let duration = if ictx.duration() > 0 {
        ictx.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE)
    } else {
        video.duration().max(0) as f64 * f64::from(video.time_base())
    };
    let audio_tracks = ictx
        .streams()
        .filter(|s| s.parameters().medium() == media::Type::Audio)
        .count();
    let bit_rate = match ictx.bit_rate() {
        rate if rate > 0 => rate as u64,
        _ if duration > 0.0 => std::fs::metadata(path)
            .map(|m| (m.len() as f64 * 8.0 / duration) as u64)
            .unwrap_or(0),
        _ => 0,
    };

    Ok(MediaInfo {
        duration,
        width: decoder.width(),
        height: decoder.height(),
        fps,
        video_codec,
        audio_tracks,
        bit_rate,
    })
}

/// The stream's frame rate: its nominal rate when the average agrees with
/// it to within 5 % (MP4's average is often off by a frame's worth, e.g.
/// 30.34 for a 30 fps clip), else the average, which is the one that means
/// something for variable frame rate recordings.
pub(crate) fn stream_fps(stream: &ffmpeg::format::stream::Stream) -> f64 {
    let valid = |r: ffmpeg::Rational| r.numerator() > 0 && r.denominator() > 0;
    let (avg, nominal) = (stream.avg_frame_rate(), stream.rate());
    match (valid(avg), valid(nominal)) {
        (true, true) => {
            let (avg, nominal) = (f64::from(avg), f64::from(nominal));
            if (avg - nominal).abs() / nominal < 0.05 {
                nominal
            } else {
                avg
            }
        }
        (true, false) => f64::from(avg),
        (false, true) => f64::from(nominal),
        (false, false) => 0.0,
    }
}
