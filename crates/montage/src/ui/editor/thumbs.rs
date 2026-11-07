//! The filmstrip and the waveform, made on background threads (and cached
//! in the clip's `.thumbs/` folder by montage-media), with the mixed
//! waveform kept for the current gains.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use montage_core::render::RgbaImage;
use montage_media::Waveform;

/// Frames in the filmstrip: a fixed count, so the cache always matches.
pub const FILMSTRIP_COUNT: usize = 32;
/// Height of a filmstrip frame, in pixels: enough for a 2x screen.
pub const FILMSTRIP_HEIGHT: u32 = 108;
pub const WAVEFORM_BUCKETS: usize = 2400;

/// Something made in the background.
pub enum Loadable<T> {
    Loading,
    Ready(T),
    Failed(String),
}

enum Message {
    Filmstrip(Result<Vec<RgbaImage>, String>),
    Waveform(Result<Waveform, String>),
}

pub struct Thumbs {
    rx: Option<Receiver<Message>>,
    pub filmstrip: Loadable<Vec<TextureHandle>>,
    pub waveform: Loadable<Waveform>,
    /// The mixed waveform and the gains it was mixed with.
    mixed: Option<(Vec<f32>, Peaks)>,
    scale: f32,
}

/// (min, max) per bucket.
type Peaks = Vec<(f32, f32)>;

impl Thumbs {
    /// Starts making the filmstrip and the waveform of `path`.
    pub fn start(ctx: &egui::Context, path: PathBuf, cache_dir: PathBuf) -> Thumbs {
        let (tx, rx) = channel();
        {
            let (tx, ctx, path, cache) = (tx.clone(), ctx.clone(), path.clone(), cache_dir.clone());
            std::thread::spawn(move || {
                let result = montage_media::filmstrip(
                    &path,
                    FILMSTRIP_COUNT,
                    FILMSTRIP_HEIGHT,
                    Some(&cache),
                )
                .map_err(|e| e.to_string());
                let _ = tx.send(Message::Filmstrip(result));
                ctx.request_repaint();
            });
        }
        {
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let result = montage_media::waveform(&path, WAVEFORM_BUCKETS, Some(&cache_dir))
                    .map_err(|e| e.to_string());
                let _ = tx.send(Message::Waveform(result));
                ctx.request_repaint();
            });
        }
        Thumbs {
            rx: Some(rx),
            filmstrip: Loadable::Loading,
            waveform: Loadable::Loading,
            mixed: None,
            scale: 1.0,
        }
    }

    /// Already made, uploaded on the first `poll`: for tests.
    #[cfg(test)]
    pub fn ready(frames: Vec<RgbaImage>, waveform: Waveform) -> Thumbs {
        let (tx, rx) = channel();
        tx.send(Message::Filmstrip(Ok(frames))).unwrap();
        tx.send(Message::Waveform(Ok(waveform))).unwrap();
        Thumbs {
            rx: Some(rx),
            filmstrip: Loadable::Loading,
            waveform: Loadable::Loading,
            mixed: None,
            scale: 1.0,
        }
    }

    /// Never done: for tests.
    #[cfg(test)]
    pub fn loading() -> Thumbs {
        Thumbs {
            rx: None,
            filmstrip: Loadable::Loading,
            waveform: Loadable::Loading,
            mixed: None,
            scale: 1.0,
        }
    }

    /// Takes in what the threads made.
    pub fn poll(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.rx else { return };
        loop {
            match rx.try_recv() {
                Ok(Message::Filmstrip(Ok(frames))) => {
                    self.filmstrip = Loadable::Ready(upload(ctx, frames));
                }
                Ok(Message::Filmstrip(Err(e))) => self.filmstrip = Loadable::Failed(e),
                Ok(Message::Waveform(Ok(waveform))) => {
                    self.scale = display_scale(&waveform);
                    self.waveform = Loadable::Ready(waveform);
                    self.mixed = None;
                }
                Ok(Message::Waveform(Err(e))) => self.waveform = Loadable::Failed(e),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // A thread died without a word (it panicked).
                    if matches!(self.filmstrip, Loadable::Loading) {
                        self.filmstrip = Loadable::Failed("internal error".into());
                    }
                    if matches!(self.waveform, Loadable::Loading) {
                        self.waveform = Loadable::Failed("internal error".into());
                    }
                    self.rx = None;
                    break;
                }
            }
        }
    }

    /// Mixes the waveform with `gains`, as the export will mix it, when
    /// they changed.
    pub fn mix(&mut self, gains: &[f32]) {
        if let Loadable::Ready(waveform) = &self.waveform
            && self.mixed.as_ref().is_none_or(|(g, _)| g != gains)
        {
            self.mixed = Some((gains.to_vec(), waveform.mixed(gains)));
        }
    }

    /// The last mix, the seconds each peak covers, and the display scale:
    /// the loudest peak at full volume fills the row, so quiet clips still
    /// show, and volume changes still show as changes.
    pub fn mixed(&self) -> Option<(&[(f32, f32)], f64, f32)> {
        match (&self.waveform, &self.mixed) {
            (Loadable::Ready(waveform), Some((_, peaks))) => {
                Some((peaks.as_slice(), waveform.bucket_seconds, self.scale))
            }
            _ => None,
        }
    }
}

/// How much to enlarge the waveform so its loudest peak, with every track
/// at 100 %, fills the row; at most 8 times, so near-silence stays flat.
pub fn display_scale(waveform: &Waveform) -> f32 {
    let loudest = waveform
        .mixed(&[])
        .iter()
        .fold(0.0f32, |m, &(lo, hi)| m.max(lo.abs()).max(hi.abs()));
    1.0 / loudest.max(0.125)
}

fn upload(ctx: &egui::Context, frames: Vec<RgbaImage>) -> Vec<TextureHandle> {
    frames
        .iter()
        .enumerate()
        .filter(|(_, f)| f.width > 0 && f.height > 0)
        .map(|(i, f)| {
            ctx.load_texture(
                format!("montage-filmstrip-{i}"),
                to_color_image(f),
                TextureOptions::LINEAR,
            )
        })
        .collect()
}

pub fn to_color_image(image: &RgbaImage) -> ColorImage {
    ColorImage::from_rgba_unmultiplied([image.width as usize, image.height as usize], &image.data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_waveforms_are_enlarged_within_reason() {
        let wave = |peak: f32| Waveform {
            bucket_seconds: 0.1,
            tracks: vec![vec![(-peak, peak), (0.0, 0.0)], vec![(-peak, peak)]],
        };
        assert_eq!(display_scale(&wave(0.25)), 2.0);
        assert_eq!(display_scale(&wave(0.5)), 1.0);
        assert_eq!(display_scale(&wave(0.001)), 8.0);
        assert_eq!(display_scale(&Waveform::default()), 8.0);
    }
}
