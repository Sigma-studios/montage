//! Every export: the Share, Full quality and GIF presets, with the edit's
//! cut, track gains and captions applied.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use montage_core::edit::Cut;
use montage_core::presets::GifSettings;
use montage_core::render::Overlay;
use montage_core::share::SharePlan;

use crate::export as pipeline;
use crate::{MediaError, MediaInfo, full, gif};

pub enum ExportPreset {
    /// H.264 two-pass + AAC under `max_bytes`, planned by
    /// `montage_core::share::plan` (which already applied the cut).
    Share { plan: SharePlan, max_bytes: u64 },
    /// The original's size and frame rate. Copies the video stream when only
    /// the cut changed (no overlay, gains all 1.0) and the cut starts on a
    /// keyframe; otherwise the GPU encoder (NVENC, AMF, QSV, VAAPI,
    /// VideoToolbox), or libx264 when there's none.
    Full,
    /// gifski, no audio, lowering the width then the frame rate until the
    /// file fits `max_bytes`.
    Gif(GifSettings),
}

pub struct ExportJob<'a> {
    pub input: &'a Path,
    /// Written through a temporary file next to it, renamed at the end.
    pub output: &'a Path,
    pub preset: ExportPreset,
    /// For Share, ignored: the plan's range is used.
    pub cut: Cut,
    /// Linear gain per audio track (missing entries 1.0; 0.0 mutes).
    pub gains: &'a [f32],
    /// The captions, drawn on every frame at the export's size.
    pub overlay: Option<&'a mut dyn Overlay>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportReport {
    pub bytes: u64,
    /// How it was made, for the status line: `stream copy`, `h264_nvenc`,
    /// `libx264 two-pass`, `gifski 400 px 15 fps`…
    pub method: String,
}

/// Runs an export. `progress` gets 0.0..=1.0, and restarts from 0.0 when
/// an export over its size target is redone smaller (Share, GIF) or a GPU
/// encoder fails and the next one takes over (Full quality). Setting
/// `cancel` stops it, between two frames, with [`MediaError::Cancelled`]
/// and leaves nothing behind.
pub fn export(
    job: ExportJob,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> Result<ExportReport, MediaError> {
    crate::init();
    let ExportJob {
        input,
        output,
        preset,
        cut,
        gains,
        overlay,
    } = job;
    let overlay = overlay.filter(|o| !o.is_empty());
    let method = match &preset {
        ExportPreset::Share { plan, max_bytes } => {
            let (_, kbps) = pipeline::share_within(
                input, output, plan, gains, *max_bytes, overlay, cancel, progress,
            )?;
            format!("libx264 two-pass {kbps} kbit/s")
        }
        ExportPreset::Full => {
            full::export_full(input, output, cut, gains, overlay, cancel, progress)?
        }
        ExportPreset::Gif(settings) => {
            gif::export_gif(input, output, settings, cut, overlay, cancel, progress)?
        }
    };
    let bytes = pipeline::file_size(output)?;
    Ok(ExportReport { bytes, method })
}

/// Bytes per pixel per frame of a gifski GIF at quality 90: game footage
/// in motion takes about 0.5, screen recordings and still scenes 0.1 or
/// less, so the middle keeps the estimate within 2.5 times of either.
const GIF_BYTES_PER_PIXEL: f64 = 0.25;

/// A quick size estimate for the export bar, without encoding.
///
/// Share: the plan's bitrates over its range. Full quality: the source's
/// bitrate over the cut, which is what a stream copy gives and about what
/// the constant quality re-encode lands on. GIF: a rough bytes per pixel
/// per frame. Share and GIF never estimate over their size target, which
/// the export holds to.
pub fn estimate_bytes(info: &MediaInfo, preset: &ExportPreset, cut: Cut) -> u64 {
    match preset {
        ExportPreset::Share { plan, max_bytes } => {
            let audio = if info.audio_tracks > 0 {
                plan.audio_kbps
            } else {
                0
            };
            let kbits = f64::from(plan.video_kbps + audio) * plan.range.duration();
            // 1 % for the container.
            ((kbits * 1000.0 / 8.0 * 1.01) as u64).min(*max_bytes)
        }
        ExportPreset::Full => {
            let seconds = cut.clamped(info.duration).duration();
            let bit_rate = if info.bit_rate > 0 {
                info.bit_rate as f64
            } else {
                // Unknown: about what a recorder's high quality gives.
                f64::from(info.width) * f64::from(info.height) * info.fps.max(1.0) * 0.15
            };
            (bit_rate * seconds / 8.0) as u64
        }
        ExportPreset::Gif(settings) => {
            if info.width == 0 || info.height == 0 {
                return 0;
            }
            let (width, height, fps) = gif::first_params(info, settings);
            let frames = (cut.clamped(info.duration).duration() * f64::from(fps)).ceil();
            let bytes = f64::from(width) * f64::from(height) * frames * GIF_BYTES_PER_PIXEL;
            (bytes as u64).min(settings.max_bytes)
        }
    }
}
