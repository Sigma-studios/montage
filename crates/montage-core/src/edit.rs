//! The edit saved next to a clip in `montage.toml`: the cut, each track's
//! volume, and the captions. The original is never modified; exports apply
//! the edit.
//!
//! The file holds no absolute paths and positions are fractions of the
//! frame, so a clip folder can be copied to another computer and opened there.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The version written by this build. Files from a newer Montage are refused
/// rather than half-read and saved back without what this build didn't know.
pub const VERSION: u32 = 1;

/// The font captions use by default, and fall back to when theirs is missing.
pub const DEFAULT_FONT: &str = "Impact";

/// Length of the fade applied at both cut points, so the edges don't pop.
pub const EDGE_FADE_SECS: f64 = 0.05;

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("can't read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("can't write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("{path} isn't a valid edit: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("{path} was saved by a newer Montage (edit version {version})")]
    TooNew { path: PathBuf, version: u32 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edit {
    pub version: u32,
    /// The part of the clip kept, or the whole clip when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut: Option<Cut>,
    /// Settings of the tracks that differ from the default (unmuted, 100 %).
    #[serde(default, rename = "track", skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<TrackSettings>,
    #[serde(default, rename = "caption", skip_serializing_if = "Vec::is_empty")]
    pub captions: Vec<Caption>,
    /// Marked as kept in the library.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub starred: bool,
}

impl Default for Edit {
    fn default() -> Self {
        Edit {
            version: VERSION,
            cut: None,
            tracks: Vec::new(),
            captions: Vec::new(),
            starred: false,
        }
    }
}

/// Start and end of the cut, in seconds from the start of the original.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Cut {
    pub start: f64,
    pub end: f64,
}

impl Cut {
    /// The cut, kept within `0..=duration` and at least one millisecond long.
    pub fn clamped(self, duration: f64) -> Cut {
        let start = self.start.clamp(0.0, duration);
        let end = self.end.clamp(start, duration);
        if end - start < 0.001 {
            Cut {
                start: 0.0,
                end: duration,
            }
        } else {
            Cut { start, end }
        }
    }

    pub fn duration(self) -> f64 {
        self.end - self.start
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TrackSettings {
    /// The audio track's position among the clip's audio tracks, from 0.
    pub index: usize,
    #[serde(default)]
    pub muted: bool,
    /// Linear gain: 1.0 leaves the track as recorded.
    #[serde(default = "one")]
    pub volume: f32,
}

impl TrackSettings {
    pub fn unchanged(index: usize) -> Self {
        TrackSettings {
            index,
            muted: false,
            volume: 1.0,
        }
    }

    /// The gain to apply when mixing down.
    pub fn gain(&self) -> f32 {
        if self.muted { 0.0 } else { self.volume }
    }

    fn is_unchanged(&self) -> bool {
        !self.muted && self.volume == 1.0
    }
}

fn one() -> f32 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Caption {
    pub text: String,
    /// A font family name, looked up among the installed fonts.
    pub font: String,
    /// Shown in capitals, whatever the text's case.
    pub uppercase: bool,
    /// Height of a line, as a fraction of the frame's height.
    pub size: f32,
    pub fill: Color,
    pub outline: Color,
    /// Outline width, as a fraction of `size`.
    pub outline_width: f32,
    pub shadow: bool,
    /// Centre of the text, as fractions of the frame's width and height.
    pub x: f32,
    pub y: f32,
    /// When the caption shows, in seconds of the original. Absent means from
    /// the start, or to the end, of the cut.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<f64>,
}

impl Default for Caption {
    /// The classic caption: Impact, white, a thick black outline, in
    /// capitals, centred near the top.
    fn default() -> Self {
        Caption {
            text: String::new(),
            font: DEFAULT_FONT.into(),
            uppercase: true,
            size: 0.1,
            fill: Color::WHITE,
            outline: Color::BLACK,
            outline_width: 0.08,
            shadow: false,
            x: 0.5,
            y: 0.12,
            start: None,
            end: None,
        }
    }
}

impl Caption {
    /// The text as drawn.
    pub fn display_text(&self) -> String {
        if self.uppercase {
            self.text.to_uppercase()
        } else {
            self.text.clone()
        }
    }

    /// Whether the caption shows at `t` seconds, within `cut`.
    pub fn visible_at(&self, t: f64, cut: Cut) -> bool {
        let start = self.start.unwrap_or(cut.start);
        let end = self.end.unwrap_or(cut.end);
        t >= start && t < end
    }
}

/// An sRGB colour with alpha, written `#rrggbb` or `#rrggbbaa`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const WHITE: Color = Color::rgb(255, 255, 255);
    pub const BLACK: Color = Color::rgb(0, 0, 0);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b, a: 255 }
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)?;
        if self.a != 255 {
            write!(f, "{:02x}", self.a)?;
        }
        Ok(())
    }
}

impl FromStr for Color {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || format!("{s:?} isn't a colour like #ffffff or #ffffff80");
        let hex = s.strip_prefix('#').ok_or_else(bad)?;
        if !(hex.len() == 6 || hex.len() == 8) || !hex.is_ascii() {
            return Err(bad());
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| bad());
        Ok(Color {
            r: byte(0)?,
            g: byte(2)?,
            b: byte(4)?,
            a: if hex.len() == 8 { byte(6)? } else { 255 },
        })
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl Edit {
    /// Reads the edit at `path`, or `None` when the clip has no edit yet.
    pub fn load(path: &Path) -> Result<Option<Edit>, EditError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(EditError::Read {
                    path: path.into(),
                    source,
                });
            }
        };
        let edit: Edit = toml::from_str(&text).map_err(|source| EditError::Parse {
            path: path.into(),
            source,
        })?;
        if edit.version > VERSION {
            return Err(EditError::TooNew {
                path: path.into(),
                version: edit.version,
            });
        }
        Ok(Some(edit))
    }

    /// Writes the edit to `path`, through a temporary file so a crash never
    /// leaves half an edit.
    pub fn save(&self, path: &Path) -> Result<(), EditError> {
        let write_err = |source| EditError::Write {
            path: path.into(),
            source,
        };
        let mut edit = self.clone();
        edit.version = VERSION;
        edit.tracks.retain(|t| !t.is_unchanged());
        edit.tracks.sort_by_key(|t| t.index);
        let text = toml::to_string_pretty(&edit).expect("an edit always serialises");

        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text).map_err(write_err)?;
        fs::rename(&tmp, path).map_err(write_err)
    }

    /// Settings of the audio track at `index`.
    pub fn track(&self, index: usize) -> TrackSettings {
        self.tracks
            .iter()
            .copied()
            .find(|t| t.index == index)
            .unwrap_or(TrackSettings::unchanged(index))
    }

    /// Sets the audio track at `index`, replacing its previous settings.
    pub fn set_track(&mut self, settings: TrackSettings) {
        self.tracks.retain(|t| t.index != settings.index);
        self.tracks.push(settings);
    }

    /// The cut, or the whole clip, kept within `duration`.
    pub fn cut_within(&self, duration: f64) -> Cut {
        self.cut
            .unwrap_or(Cut {
                start: 0.0,
                end: duration,
            })
            .clamped(duration)
    }

    /// True when the only change is the cut: the video can then be copied
    /// unchanged by the Full quality preset.
    pub fn only_cuts(&self) -> bool {
        self.captions.is_empty() && self.tracks.iter().all(TrackSettings::is_unchanged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_edit_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(Edit::load(&tmp.path().join("montage.toml")).unwrap(), None);
    }

    #[test]
    fn edit_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("montage.toml");
        let mut edit = Edit {
            cut: Some(Cut {
                start: 2.5,
                end: 14.0,
            }),
            ..Edit::default()
        };
        edit.set_track(TrackSettings {
            index: 1,
            muted: true,
            volume: 1.0,
        });
        edit.set_track(TrackSettings {
            index: 0,
            muted: false,
            volume: 0.6,
        });
        edit.captions.push(Caption {
            text: "nice shot".into(),
            end: Some(5.0),
            ..Caption::default()
        });
        edit.save(&path).unwrap();

        let loaded = Edit::load(&path).unwrap().unwrap();
        assert_eq!(loaded.cut, edit.cut);
        assert_eq!(loaded.track(0).gain(), 0.6);
        assert_eq!(loaded.track(1).gain(), 0.0);
        assert_eq!(loaded.track(2), TrackSettings::unchanged(2));
        assert_eq!(loaded.captions, edit.captions);
        assert!(!loaded.only_cuts());
    }

    #[test]
    fn unchanged_tracks_are_not_written() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("montage.toml");
        let mut edit = Edit::default();
        edit.set_track(TrackSettings::unchanged(0));
        edit.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("track"), "{text}");
        assert!(Edit::load(&path).unwrap().unwrap().only_cuts());
    }

    #[test]
    fn short_hand_file_reads_with_defaults() {
        let edit: Edit = toml::from_str(
            r##"
            version = 1
            [cut]
            start = 1.0
            end = 3.0
            [[track]]
            index = 1
            muted = true
            [[caption]]
            text = "gg"
            fill = "#ffcc00"
            "##,
        )
        .unwrap();
        assert_eq!(edit.track(1).volume, 1.0);
        let caption = &edit.captions[0];
        assert_eq!(caption.font, "Impact");
        assert_eq!(caption.fill, Color::rgb(255, 204, 0));
        assert_eq!(caption.outline, Color::BLACK);
        assert_eq!(caption.display_text(), "GG");
    }

    #[test]
    fn newer_edits_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("montage.toml");
        fs::write(&path, "version = 2\n").unwrap();
        assert!(matches!(
            Edit::load(&path),
            Err(EditError::TooNew { version: 2, .. })
        ));
    }

    #[test]
    fn colours_parse_and_print() {
        assert_eq!("#ffffff".parse(), Ok(Color::WHITE));
        assert_eq!(
            "#00000080".parse::<Color>().unwrap(),
            Color {
                r: 0,
                g: 0,
                b: 0,
                a: 128
            }
        );
        assert!("ffffff".parse::<Color>().is_err());
        assert!("#fff".parse::<Color>().is_err());
        assert!("#ééé".parse::<Color>().is_err());
        assert_eq!(Color::rgb(255, 204, 0).to_string(), "#ffcc00");
    }

    #[test]
    fn cut_is_kept_within_the_clip() {
        let c = Cut {
            start: -1.0,
            end: 99.0,
        }
        .clamped(30.0);
        assert_eq!((c.start, c.end), (0.0, 30.0));
        let empty = Cut {
            start: 10.0,
            end: 10.0,
        }
        .clamped(30.0);
        assert_eq!((empty.start, empty.end), (0.0, 30.0));
        assert_eq!(Edit::default().cut_within(12.0).duration(), 12.0);
    }

    #[test]
    fn captions_follow_the_cut_by_default() {
        let cut = Cut {
            start: 2.0,
            end: 8.0,
        };
        let whole = Caption::default();
        assert!(!whole.visible_at(1.9, cut));
        assert!(whole.visible_at(2.0, cut));
        assert!(!whole.visible_at(8.0, cut));
        let timed = Caption {
            start: Some(4.0),
            ..Caption::default()
        };
        assert!(!timed.visible_at(3.0, cut));
        assert!(timed.visible_at(4.0, cut));
    }
}
