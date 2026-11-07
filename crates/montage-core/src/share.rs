//! Planning the Share preset: H.264 + AAC under a size budget, at 1080p or
//! 720p and at most 60 fps. When the clip is too long for the budget even at
//! the lowest bitrate, a replay keeps its end and a recording keeps its start.
//!
//! The rules are `gsr-clip-export`'s, with its numbers as defaults. They live
//! in the `[share]` section of `config.toml`.

use serde::{Deserialize, Serialize};

use crate::edit::Cut;

/// Share of the budget left for the container and x264's rate control
/// missing its target.
const SAFETY: f64 = 0.04;

/// The `[share]` section of `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShareSettings {
    /// The size budget, in bytes.
    pub max_bytes: u64,
    /// Frame rate cap.
    pub fps: u32,
    /// AAC bitrate, in kbit/s.
    pub audio_kbps: u32,
    /// The lowest video bitrate, in kbit/s. A clip that would need less is
    /// cut instead.
    pub min_video_kbps: u32,
    /// From this video bitrate up, in kbit/s, the export is 1080p instead
    /// of 720p.
    pub hd_video_kbps: u32,
    /// The highest video bitrate, in kbit/s, however short the clip.
    pub max_video_kbps: u32,
}

impl Default for ShareSettings {
    fn default() -> Self {
        ShareSettings {
            max_bytes: 20 * 1024 * 1024,
            fps: 60,
            audio_kbps: 128,
            min_video_kbps: 2000,
            hd_video_kbps: 4500,
            max_video_kbps: 8000,
        }
    }
}

/// Whether the clip came from the replay buffer or a regular recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipKind {
    /// The moment that matters is at the end: trimming keeps the end.
    Replay,
    /// Trimming keeps the start.
    Regular,
}

impl ClipKind {
    /// Guesses from the clip's name, as gpu-screen-recorder names replays
    /// `Replay_…`.
    pub fn guess(name: &str) -> ClipKind {
        if name.starts_with("Replay_") {
            ClipKind::Replay
        } else {
            ClipKind::Regular
        }
    }
}

/// What the plan needs to know about the original.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Source {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SharePlan {
    /// The part of the original exported.
    pub range: Cut,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// x264's two-pass target.
    pub video_kbps: u32,
    pub audio_kbps: u32,
}

impl SharePlan {
    /// The trim applied on top of the cut, in seconds, when the budget forced
    /// one.
    pub fn trimmed(&self, cut: Cut) -> f64 {
        cut.duration() - self.range.duration()
    }
}

/// Plans the Share export of `cut` (the whole clip when `None`).
pub fn plan(rules: &ShareSettings, source: Source, kind: ClipKind, cut: Option<Cut>) -> SharePlan {
    let cut = cut
        .unwrap_or(Cut {
            start: 0.0,
            end: source.duration,
        })
        .clamped(source.duration);

    let budget_kbits = rules.max_bytes as f64 * 8.0 / 1000.0 * (1.0 - SAFETY);
    let audio = f64::from(rules.audio_kbps);

    // The longest stretch that still gets the lowest bitrate.
    let longest = budget_kbits / (f64::from(rules.min_video_kbps) + audio);
    let range = if cut.duration() <= longest {
        cut
    } else {
        match kind {
            ClipKind::Replay => Cut {
                start: cut.end - longest,
                end: cut.end,
            },
            ClipKind::Regular => Cut {
                start: cut.start,
                end: cut.start + longest,
            },
        }
    };

    let video_kbps = (budget_kbits / range.duration() - audio).min(f64::from(rules.max_video_kbps));
    let target_height = if video_kbps >= f64::from(rules.hd_video_kbps) {
        1080
    } else {
        720
    };
    let (width, height) = scale_to_height(source.width, source.height, target_height);

    SharePlan {
        range,
        width,
        height,
        fps: source.fps.min(f64::from(rules.fps)),
        video_kbps: video_kbps.floor() as u32,
        audio_kbps: rules.audio_kbps,
    }
}

/// The size with the given height, keeping the aspect ratio, never upscaling,
/// and with even sides as H.264's 4:2:0 needs.
fn scale_to_height(width: u32, height: u32, target: u32) -> (u32, u32) {
    let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(2);
    if height <= target {
        return (even(f64::from(width)), even(f64::from(height)));
    }
    let scale = f64::from(target) / f64::from(height);
    (even(f64::from(width) * scale), even(f64::from(target)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: ShareSettings = ShareSettings {
        max_bytes: 20 * 1024 * 1024,
        fps: 60,
        audio_kbps: 128,
        min_video_kbps: 2000,
        hd_video_kbps: 4500,
        max_video_kbps: 8000,
    };

    const QHD: Source = Source {
        duration: 30.0,
        width: 2560,
        height: 1440,
        fps: 144.0,
    };

    fn size_bytes(plan: &SharePlan) -> f64 {
        f64::from(plan.video_kbps + plan.audio_kbps) * 1000.0 / 8.0 * plan.range.duration()
    }

    #[test]
    fn short_clip_is_1080p60_under_the_cap() {
        let p = plan(
            &RULES,
            Source {
                duration: 15.0,
                ..QHD
            },
            ClipKind::Replay,
            None,
        );
        assert_eq!((p.width, p.height), (1920, 1080));
        assert_eq!(p.fps, 60.0);
        assert_eq!(p.range.duration(), 15.0);
        assert!(size_bytes(&p) <= RULES.max_bytes as f64);
    }

    #[test]
    fn short_clip_is_capped_at_the_highest_bitrate() {
        let p = plan(
            &RULES,
            Source {
                duration: 5.0,
                ..QHD
            },
            ClipKind::Replay,
            None,
        );
        assert_eq!(p.video_kbps, 8000);
    }

    #[test]
    fn hd_starts_at_its_bitrate() {
        // 161,061 kbit of budget: 1080p down to about 34.8 s, 720p after.
        let at = |duration| plan(&RULES, Source { duration, ..QHD }, ClipKind::Replay, None);
        assert_eq!(at(34.0).height, 1080);
        assert!(at(34.0).video_kbps >= 4500);
        assert_eq!(at(36.0).height, 720);
    }

    #[test]
    fn longer_clip_drops_to_720p() {
        let p = plan(
            &RULES,
            Source {
                duration: 60.0,
                ..QHD
            },
            ClipKind::Replay,
            None,
        );
        assert_eq!((p.width, p.height), (1280, 720));
        assert_eq!(p.range.duration(), 60.0);
        assert!(size_bytes(&p) <= RULES.max_bytes as f64);
    }

    #[test]
    fn too_long_replay_keeps_its_end() {
        let p = plan(
            &RULES,
            Source {
                duration: 300.0,
                ..QHD
            },
            ClipKind::Replay,
            None,
        );
        assert_eq!(p.range.end, 300.0);
        assert!(p.range.start > 0.0);
        assert!(size_bytes(&p) <= RULES.max_bytes as f64);
    }

    #[test]
    fn too_long_recording_keeps_its_start() {
        let p = plan(
            &RULES,
            Source {
                duration: 300.0,
                ..QHD
            },
            ClipKind::Regular,
            None,
        );
        assert_eq!(p.range.start, 0.0);
        assert!(p.range.end < 300.0);
    }

    #[test]
    fn the_cut_is_respected_and_trimmed_within() {
        let cut = Cut {
            start: 20.0,
            end: 280.0,
        };
        let p = plan(
            &RULES,
            Source {
                duration: 300.0,
                ..QHD
            },
            ClipKind::Replay,
            Some(cut),
        );
        assert_eq!(p.range.end, 280.0);
        assert!(p.range.start > 20.0);
        assert!(p.trimmed(cut) > 0.0);
    }

    #[test]
    fn small_sources_are_not_upscaled() {
        let p = plan(
            &RULES,
            Source {
                duration: 10.0,
                width: 1280,
                height: 720,
                fps: 30.0,
            },
            ClipKind::Regular,
            None,
        );
        assert_eq!((p.width, p.height), (1280, 720));
        assert_eq!(p.fps, 30.0);
    }

    #[test]
    fn odd_aspect_ratios_get_even_sides() {
        // 21:9 ultrawide.
        assert_eq!(scale_to_height(3440, 1440, 1080), (2580, 1080));
        assert_eq!(scale_to_height(1001, 721, 720), (1000, 720));
        assert_eq!(scale_to_height(853, 479, 720), (854, 480));
    }

    #[test]
    fn replays_are_recognised_by_name() {
        assert_eq!(
            ClipKind::guess("Replay_2026-11-07_21-03-11"),
            ClipKind::Replay
        );
        assert_eq!(
            ClipKind::guess("Video_2026-11-07_21-03-11"),
            ClipKind::Regular
        );
    }
}
