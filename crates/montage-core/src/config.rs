//! Montage's config, in the config folder of each system:
//! `~/.config/montage/config.toml` on Linux,
//! `%APPDATA%\montage\config.toml` on Windows,
//! `~/Library/Application Support/montage/config.toml` on macOS.
//!
//! Every key is optional; a missing file means the defaults. On Linux the
//! Home Manager module writes the first version, then Montage's settings
//! page owns it. Every command reads the same file, so the recorder, `save`
//! and `share` can never disagree.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::presets::GifSettings;
use crate::recorder::RecorderSettings;
use crate::share::ShareSettings;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("can't read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{path} isn't a valid config: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("can't write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Folders the library shows clips from. `~` stands for the home folder.
    /// When absent: the recorder's two folders on Linux, the Videos (Movies)
    /// folder elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library: Option<Vec<PathBuf>>,
    /// Names of the audio tracks, in order. Unnamed tracks are "Track 1",
    /// "Track 2", and so on.
    pub track_names: Vec<String>,
    pub recorder: RecorderSettings,
    pub share: ShareSettings,
    pub gif: GifSettings,
    pub sounds: SoundSettings,
    pub editor: EditorSettings,
}

/// The `[editor]` section: what the editor remembers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditorSettings {
    /// Fonts picked lately, most recent first; shown under Impact in the
    /// font picker.
    pub recent_fonts: Vec<String>,
}

/// The `[sounds]` section.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SoundSettings {
    pub enabled: bool,
    /// Linear volume, from 0 to 1.
    pub volume: f32,
}

impl Default for SoundSettings {
    fn default() -> Self {
        SoundSettings {
            enabled: true,
            volume: 0.6,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            library: None,
            track_names: default_track_names(),
            recorder: RecorderSettings::default(),
            share: ShareSettings::default(),
            gif: GifSettings::default(),
            sounds: SoundSettings::default(),
            editor: EditorSettings::default(),
        }
    }
}

/// The user's Videos folder (Movies on macOS).
pub(crate) fn videos_dir() -> Option<PathBuf> {
    directories::UserDirs::new().and_then(|d| d.video_dir().map(Path::to_path_buf))
}

/// The Linux recorder writes the desktop, then the mic. Other recorders'
/// track orders vary, so they get numbered names.
fn default_track_names() -> Vec<String> {
    if cfg!(target_os = "linux") {
        vec!["Game".into(), "Mic".into()]
    } else {
        Vec::new()
    }
}

impl Config {
    /// Where the config lives on this system.
    pub fn path() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|d| d.config_dir().join("montage").join("config.toml"))
    }

    /// Reads the config at the usual place, or the defaults when there's none.
    pub fn load() -> Result<Config, ConfigError> {
        match Config::path() {
            Some(path) => Config::load_from(&path),
            None => Ok(Config::default()),
        }
    }

    pub fn load_from(path: &Path) -> Result<Config, ConfigError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.into(),
                    source,
                });
            }
        };
        let mut config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.into(),
            source,
        })?;
        if let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) {
            config.expand_home(&home);
        }
        Ok(config)
    }

    /// Writes the config to `path`, through a temporary file.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        let write_err = |source| ConfigError::Write {
            path: path.into(),
            source,
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(write_err)?;
        }
        let text = toml::to_string_pretty(self).expect("a config always serialises");
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text).map_err(write_err)?;
        fs::rename(&tmp, path).map_err(write_err)
    }

    fn expand_home(&mut self, home: &Path) {
        if let Some(library) = &mut self.library {
            for path in library {
                *path = expand_home(path, home);
            }
        }
        self.recorder.replay_dir = expand_home(&self.recorder.replay_dir, home);
        self.recorder.record_dir = expand_home(&self.recorder.record_dir, home);
    }

    /// The folders the library shows.
    pub fn library_folders(&self) -> Vec<PathBuf> {
        if let Some(library) = &self.library {
            return library.clone();
        }
        if cfg!(target_os = "linux") {
            vec![
                self.recorder.replay_dir.clone(),
                self.recorder.record_dir.clone(),
            ]
        } else {
            videos_dir().into_iter().collect()
        }
    }

    /// The name shown for the audio track at `index`, from 0.
    pub fn track_name(&self, index: usize) -> String {
        self.track_names
            .get(index)
            .cloned()
            .unwrap_or_else(|| format!("Track {}", index + 1))
    }
}

fn expand_home(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::Codec;

    #[test]
    fn missing_file_gives_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config::load_from(&tmp.path().join("config.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn keys_are_read_and_home_is_expanded() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            r#"
            library = ["~/Clips", "/srv/clips"]
            track_names = ["Desktop"]

            [recorder]
            monitor = "DP-5"
            fps = 144
            replay_dir = "~/Videos/Replays"

            [share]
            max_bytes = 10485760

            [sounds]
            enabled = false
            "#,
        )
        .unwrap();
        let config = Config::load_from(&path).unwrap();
        let library = config.library_folders();
        assert!(!library[0].starts_with("~"));
        assert!(library[0].ends_with("Clips"));
        assert_eq!(library[1], PathBuf::from("/srv/clips"));
        assert_eq!(config.track_name(0), "Desktop");
        assert_eq!(config.track_name(1), "Track 2");
        assert_eq!(config.recorder.monitor, "DP-5");
        assert_eq!(config.recorder.fps, 144);
        assert_eq!(config.recorder.codec, Codec::Hevc);
        assert!(!config.recorder.replay_dir.starts_with("~"));
        assert_eq!(config.share.max_bytes, 10 * 1024 * 1024);
        assert_eq!(config.share.min_video_kbps, 2000);
        assert!(!config.sounds.enabled);
        assert_eq!(config.sounds.volume, 0.6);
    }

    #[test]
    fn unknown_keys_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        for text in ["libary = []\n", "[recorder]\nfsp = 60\n"] {
            fs::write(&path, text).unwrap();
            assert!(
                matches!(Config::load_from(&path), Err(ConfigError::Parse { .. })),
                "{text}"
            );
        }
    }

    #[test]
    fn config_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("montage").join("config.toml");
        let mut config = Config::default();
        config.recorder.monitor = "HDMI-A-1".into();
        config.recorder.codec = Codec::Av1;
        config.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), config);
    }

    #[test]
    fn library_defaults_follow_the_system() {
        let config = Config::default();
        let library = config.library_folders();
        if cfg!(target_os = "linux") {
            assert_eq!(
                library,
                [config.recorder.replay_dir, config.recorder.record_dir]
            );
        } else {
            assert!(library.len() <= 1);
        }
    }
}
