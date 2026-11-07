//! The tiles' thumbnails and lengths, made by a few worker threads so the
//! library opens at once. The view says which clips it wants, visible
//! tiles first, every frame; the workers take them from the front.

use std::collections::{HashSet, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

use eframe::egui;
use montage_core::Clip;
use montage_core::render::RgbaImage;

/// What a worker found out about a clip.
#[derive(Debug, Clone, Default)]
pub struct Media {
    pub image: Option<RgbaImage>,
    /// Seconds.
    pub duration: Option<f64>,
    pub error: Option<String>,
}

/// Makes the media of one clip. Runs on a worker thread.
pub type LoadFn = Arc<dyn Fn(&Clip) -> Media + Send + Sync>;

/// Height of the thumbnails asked for, in pixels: a tile's thumbnail (130
/// points tall) sharp at 2× scale.
pub const THUMB_HEIGHT: u32 = 270;

/// The real thing: a frame about a third in, cached in the clip's
/// `.thumbs` folder only when the clip already has a folder (the library
/// never moves files), and the clip's length.
pub fn load_media(clip: &Clip) -> Media {
    let cache = clip.in_folder.then(|| clip.thumbs_dir());
    let mut errors = Vec::new();
    let image = montage_media::thumbnail(&clip.original, THUMB_HEIGHT, cache.as_deref())
        .map_err(|e| errors.push(e.to_string()))
        .ok();
    let duration = montage_media::probe(&clip.original)
        .map(|info| info.duration)
        .map_err(|e| errors.push(e.to_string()))
        .ok();
    Media {
        image,
        duration,
        error: (!errors.is_empty()).then(|| errors.join("; ")),
    }
}

struct Queue {
    wanted: VecDeque<Clip>,
    busy: HashSet<PathBuf>,
    stop: bool,
}

struct Inner {
    queue: Mutex<Queue>,
    wake: Condvar,
}

pub struct Loader {
    inner: Arc<Inner>,
    results: Receiver<(PathBuf, Media)>,
}

impl Loader {
    pub fn new(ctx: &egui::Context, workers: usize, load: LoadFn) -> Loader {
        let inner = Arc::new(Inner {
            queue: Mutex::new(Queue {
                wanted: VecDeque::new(),
                busy: HashSet::new(),
                stop: false,
            }),
            wake: Condvar::new(),
        });
        let (tx, results) = mpsc::channel();
        for n in 0..workers.max(1) {
            let inner = inner.clone();
            let tx = tx.clone();
            let ctx = ctx.clone();
            let load = load.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("montage-thumbs-{n}"))
                .spawn(move || work(&inner, &tx, &ctx, &*load));
            if let Err(e) = spawned {
                eprintln!("montage: can't start a thumbnail worker: {e}");
            }
        }
        Loader { inner, results }
    }

    /// Replaces what's waiting with `clips`, in that order. Clips being
    /// made already are left out.
    pub fn want(&self, clips: impl IntoIterator<Item = Clip>) {
        let mut queue = self.inner.queue.lock().unwrap();
        let busy = std::mem::take(&mut queue.busy);
        queue.wanted = clips
            .into_iter()
            .filter(|c| !busy.contains(&c.original))
            .collect();
        queue.busy = busy;
        if !queue.wanted.is_empty() {
            self.inner.wake.notify_all();
        }
    }

    /// What the workers finished since the last call, by original path.
    pub fn finished(&self) -> Vec<(PathBuf, Media)> {
        self.results.try_iter().collect()
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        let mut queue = self.inner.queue.lock().unwrap();
        queue.stop = true;
        queue.wanted.clear();
        self.inner.wake.notify_all();
    }
}

fn work(inner: &Inner, tx: &Sender<(PathBuf, Media)>, ctx: &egui::Context, load: &LoadFnRef) {
    loop {
        let clip = {
            let mut queue = inner.queue.lock().unwrap();
            loop {
                if queue.stop {
                    return;
                }
                if let Some(clip) = queue.wanted.pop_front() {
                    queue.busy.insert(clip.original.clone());
                    break clip;
                }
                queue = inner.wake.wait(queue).unwrap();
            }
        };
        // A decoder that panics loses one thumbnail, not the worker.
        let media =
            panic::catch_unwind(AssertUnwindSafe(|| load(&clip))).unwrap_or_else(|_| Media {
                error: Some("no preview".into()),
                ..Media::default()
            });
        let stop = {
            let mut queue = inner.queue.lock().unwrap();
            queue.busy.remove(&clip.original);
            queue.stop
        };
        if stop || tx.send((clip.original, media)).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

type LoadFnRef = dyn Fn(&Clip) -> Media + Send + Sync;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn clip(name: &str) -> Clip {
        Clip {
            original: PathBuf::from(format!("/clips/{name}.mp4")),
            in_folder: false,
        }
    }

    #[test]
    fn workers_make_what_is_wanted_and_survive_panics() {
        let ctx = egui::Context::default();
        let load: LoadFn = Arc::new(|clip: &Clip| {
            if clip.name() == "bad" {
                panic!("decoder exploded");
            }
            Media {
                image: Some(RgbaImage::new(16, 9)),
                duration: Some(12.0),
                error: None,
            }
        });
        let loader = Loader::new(&ctx, 2, load);
        loader.want([clip("a"), clip("bad"), clip("b")]);

        let mut done = Vec::new();
        let started = Instant::now();
        while done.len() < 3 && started.elapsed() < Duration::from_secs(5) {
            done.extend(loader.finished());
            std::thread::sleep(Duration::from_millis(5));
        }
        done.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(done.len(), 3);
        assert_eq!(done[0].0, PathBuf::from("/clips/a.mp4"));
        assert_eq!(done[0].1.duration, Some(12.0));
        assert!(done[1].1.error.is_none());
        assert_eq!(done[2].0, PathBuf::from("/clips/bad.mp4"));
        assert!(done[2].1.error.is_some());
        assert!(done[2].1.image.is_none());
    }
}
