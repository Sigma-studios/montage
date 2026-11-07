//! The GIF export, with gifski: no audio, the frames scaled and captioned
//! by the same pipeline as the MP4 exports, then quantised by gifski on its
//! own thread. Over the size target, it's redone smaller: a lower width,
//! and from below 320 px or every other attempt a lower frame rate.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use ffmpeg_next::format::Pixel;
use ffmpeg_next::{Rational, frame};
use montage_core::edit::Cut;
use montage_core::presets::GifSettings;
use montage_core::render::Overlay;
use rgb::FromSlice;

use crate::export::{PartFile, VideoSpec, check, file_size, run_video};
use crate::{MediaError, MediaInfo, init};

/// Attempts before giving up on the size target.
const ATTEMPTS: usize = 5;
/// The frame rate isn't lowered below this.
const MIN_FPS: u32 = 10;
/// Below this width, each attempt lowers the frame rate too.
const SMALL_WIDTH: u32 = 320;
/// GIF delays are in hundredths of a second, so 50 fps is the most a GIF
/// can show.
const MAX_FPS: u32 = 50;
/// gifski's quality, 1-100: its own default.
const QUALITY: u8 = 90;

/// The size and frame rate of a GIF export's first attempt: `settings`,
/// within the source's width and frame rate. Height keeps the aspect ratio.
pub(crate) fn first_params(info: &MediaInfo, settings: &GifSettings) -> (u32, u32, u32) {
    let width = settings.width.clamp(16, info.width.max(16));
    let source_fps = if info.fps > 0.0 {
        info.fps.ceil() as u32
    } else {
        MAX_FPS
    };
    let fps = settings.fps.clamp(1, MAX_FPS.min(source_fps.max(1)));
    (width, height_for(info, width), fps)
}

fn height_for(info: &MediaInfo, width: u32) -> u32 {
    ((f64::from(width) * f64::from(info.height) / f64::from(info.width.max(1))).round() as u32)
        .max(1)
}

/// Exports `cut` of `input` as a GIF under `settings.max_bytes`. Returns
/// the final parameters, e.g. `gifski 384 px 15 fps`.
pub(crate) fn export_gif(
    input: &Path,
    output: &Path,
    settings: &GifSettings,
    cut: Cut,
    mut overlay: Option<&mut (dyn Overlay + '_)>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<String, MediaError> {
    init();
    let info = crate::probe(input)?;
    let range = cut.clamped(info.duration);
    if range.duration() <= 0.0 {
        return Err(MediaError::EmptyRange);
    }
    let (mut width, mut height, mut fps) = first_params(&info, settings);
    let mut part = PartFile::new(output);

    for attempt in 1..=ATTEMPTS {
        progress(0.0);
        let spec = VideoSpec {
            range,
            width,
            height,
            fps: Rational::new(fps as i32, 1),
            format: Pixel::RGBA,
        };
        encode(
            input,
            &part.path,
            spec,
            overlay.as_deref_mut(),
            cancel,
            progress,
        )?;
        let size = file_size(&part.path)?;
        if size <= settings.max_bytes {
            part.commit(output)?;
            progress(1.0);
            return Ok(format!("gifski {width} px {fps} fps"));
        }
        if attempt == ATTEMPTS {
            return Err(MediaError::OverBudget {
                size,
                max_bytes: settings.max_bytes,
            });
        }
        // The size goes with the area: aim a little under the target, but
        // by at least 0.8 and at most half at a time.
        let ratio = settings.max_bytes as f64 / size as f64;
        let factor = (ratio * 0.9).sqrt().clamp(0.5, 0.8);
        width = ((f64::from(width) * factor).round() as u32).max(16);
        height = height_for(&info, width);
        if fps > MIN_FPS && (width < SMALL_WIDTH || attempt % 2 == 0) {
            fps = ((f64::from(fps) * 0.75).round() as u32).max(MIN_FPS);
        }
    }
    unreachable!("the last attempt returns")
}

/// One gifski encode of `spec` into `path`.
fn encode(
    input: &Path,
    path: &Path,
    spec: VideoSpec,
    overlay: Option<&mut (dyn Overlay + '_)>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<(), MediaError> {
    let (collector, writer) = gifski::new(gifski::Settings {
        width: None,
        height: None,
        quality: QUALITY,
        fast: false,
        repeat: gifski::Repeat::Infinite,
    })
    .map_err(|e| MediaError::Gif(e.to_string()))?;
    let file = File::create(path).map_err(|source| MediaError::Io {
        action: "can't create",
        path: path.into(),
        source,
    })?;
    let fps = f64::from(spec.fps);

    std::thread::scope(|scope| {
        let written = scope.spawn(move || {
            let mut reporter = Reporter { cancel };
            writer.write(BufWriter::new(file), &mut reporter)
        });

        let mut index = 0usize;
        let decoded = run_video(
            input,
            spec,
            overlay,
            cancel,
            &mut |frame| {
                let image = to_image(frame);
                collector
                    .add_frame_rgba(index, image, index as f64 / fps)
                    // The writer stopped: its error says why.
                    .map_err(|e| MediaError::Gif(e.to_string()))?;
                index += 1;
                Ok(())
            },
            progress,
        );
        // No more frames: the writer finishes.
        drop(collector);
        let written = written.join().unwrap_or(Err(gifski::Error::ThreadSend));

        check(cancel)?;
        match (decoded, written) {
            (Err(MediaError::Gif(_)), Err(e)) | (Ok(()), Err(e)) => {
                Err(MediaError::Gif(e.to_string()))
            }
            (Err(e), _) => Err(e),
            (Ok(()), Ok(())) => Ok(()),
        }
    })
}

/// An RGBA frame as gifski takes it.
fn to_image(frame: &frame::Video) -> imgref::ImgVec<rgb::RGBA8> {
    let (width, height) = (frame.width() as usize, frame.height() as usize);
    let stride = frame.stride(0);
    let data = frame.data(0);
    let mut pixels = Vec::with_capacity(width * height);
    for y in 0..height {
        pixels.extend_from_slice(data[y * stride..y * stride + width * 4].as_rgba());
    }
    imgref::Img::new(pixels, width, height)
}

/// Stops gifski when the export is cancelled.
struct Reporter<'a> {
    cancel: &'a AtomicBool,
}

impl gifski::progress::ProgressReporter for Reporter<'_> {
    fn increase(&mut self) -> bool {
        check(self.cancel).is_ok()
    }
}
