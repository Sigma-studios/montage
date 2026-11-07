//! gpu-screen-recorder's settings and the rules of the commands that drive
//! it (Linux only at run time, but plain logic, so tested everywhere).
//!
//! The `[recorder]` section of `config.toml` is turned into gpu-screen-
//! recorder's command line by `montage recorder run`; `montage save` caps its
//! length with the same `replay_seconds`, so the two can never disagree.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// The `application.name` Montage's sounds play under. The desktop track
/// records every application but this one, which keeps the sounds out of
/// the clips.
pub const CHIME_APP: &str = "gsr-chime";

/// In `$XDG_RUNTIME_DIR`: exists while a manual recording runs. Read by the
/// top-bar applet, so the name and behaviour are fixed.
pub const RECORDING_FLAG: &str = "gsr-recording";

/// In `$XDG_RUNTIME_DIR`: the recorder's control socket.
pub const IPC_SOCKET: &str = "gsr.sock";

/// In `$XDG_RUNTIME_DIR`: written by `save … edit`, consumed by `on-save`.
pub const EDIT_FLAG: &str = "montage-edit-next";

/// How long the edit flag stays valid. An older one comes from a save that
/// failed and is ignored.
pub const EDIT_FLAG_LIFETIME: Duration = Duration::from_secs(10);

/// The name gpu-screen-recorder runs as its `-sc` hook. Montage runs as
/// `on-save` when started under this name, so the hook is a plain path
/// whether or not the recorder splits arguments.
pub const ON_SAVE_NAME: &str = "montage-on-save";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Hevc,
    H264,
    Av1,
}

impl Codec {
    fn flag(self) -> &'static str {
        match self {
            Codec::Hevc => "hevc",
            Codec::H264 => "h264",
            Codec::Av1 => "av1",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplayStorage {
    Ram,
    Disk,
}

/// The `[recorder]` section of `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecorderSettings {
    /// The output captured: a connector name like `DP-5`, or `screen` for
    /// the first one.
    pub monitor: String,
    pub fps: u32,
    pub codec: Codec,
    /// Constant bitrate, in kbit/s.
    pub bitrate_kbps: u32,
    /// Length of the replay buffer, and the cap of `montage save N`.
    pub replay_seconds: u32,
    pub replay_storage: ReplayStorage,
    pub replay_dir: PathBuf,
    pub record_dir: PathBuf,
    /// One entry per audio track, in order, in gpu-screen-recorder's `-a`
    /// syntax: `default_output`, `default_input`, `app:firefox`, or several
    /// joined with `|` into one track. Montage's own sounds are always left
    /// out of tracks that record the desktop.
    pub audio: Vec<String>,
}

impl Default for RecorderSettings {
    fn default() -> Self {
        let videos = crate::config::videos_dir().unwrap_or_default();
        RecorderSettings {
            monitor: "screen".into(),
            fps: 60,
            codec: Codec::Hevc,
            bitrate_kbps: 60_000,
            replay_seconds: 60,
            replay_storage: ReplayStorage::Ram,
            replay_dir: videos.join("Replays"),
            record_dir: videos.join("Recordings"),
            audio: vec!["default_output".into(), "default_input".into()],
        }
    }
}

impl RecorderSettings {
    /// gpu-screen-recorder's arguments, without the program name. `on_save`
    /// is the `-sc` hook and `ipc` the control socket.
    pub fn args(&self, on_save: &Path, ipc: &Path) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        let mut push = |flag: &str, value: OsString| {
            args.push(flag.into());
            args.push(value);
        };
        push("-w", self.monitor.clone().into());
        push("-f", self.fps.to_string().into());
        push("-k", self.codec.flag().into());
        push("-bm", "cbr".into());
        push("-q", self.bitrate_kbps.to_string().into());
        for track in &self.audio {
            push("-a", without_chime(track).into());
        }
        push("-c", "mp4".into());
        push("-r", self.replay_seconds.to_string().into());
        push(
            "-replay-storage",
            match self.replay_storage {
                ReplayStorage::Ram => "ram",
                ReplayStorage::Disk => "disk",
            }
            .into(),
        );
        push("-df", "yes".into());
        push("-o", self.replay_dir.clone().into());
        push("-ro", self.record_dir.clone().into());
        push("-sc", on_save.into());
        push("-ipc", ipc.into());
        args
    }

    /// Megabytes the buffer holds at the configured bitrate.
    pub fn replay_megabytes(&self) -> f64 {
        f64::from(self.bitrate_kbps) * f64::from(self.replay_seconds) / 8.0 / 1000.0
    }
}

/// The track with Montage's sounds left out: the default output becomes
/// "every application but the sounds", and an `app-inverse:` list gets the
/// sounds added to what it leaves out.
pub fn without_chime(track: &str) -> String {
    let chime = format!("app-inverse:{CHIME_APP}");
    let mut parts: Vec<String> = track
        .split('|')
        .map(|part| match part {
            "default_output" => chime.clone(),
            other => other.to_string(),
        })
        .collect();
    let leaves_apps_out = parts.iter().any(|p| p.starts_with("app-inverse:"));
    if leaves_apps_out && !parts.contains(&chime) {
        parts.push(chime);
    }
    parts.dedup();
    parts.join("|")
}

/// What `montage save [N] [edit]` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveRequest {
    pub seconds: u32,
    pub edit: bool,
}

/// Reads `save`'s arguments, in either order: `15 edit`, `edit 15`, `edit`,
/// `15`, or nothing. The length defaults to, and is capped at, the replay
/// buffer's. On error, returns the argument that isn't a number of seconds.
pub fn parse_save_args<S: AsRef<str>>(
    args: &[S],
    replay_seconds: u32,
) -> Result<SaveRequest, String> {
    let mut seconds = None;
    let mut edit = false;
    for arg in args {
        let arg = arg.as_ref();
        if arg == "edit" {
            edit = true;
            continue;
        }
        let valid =
            !arg.is_empty() && arg.bytes().all(|b| b.is_ascii_digit()) && !arg.starts_with('0');
        if !valid || seconds.is_some() {
            return Err(arg.to_string());
        }
        // Too many digits for a u32 is still "more than the buffer".
        seconds = Some(arg.parse::<u32>().unwrap_or(u32::MAX));
    }
    Ok(SaveRequest {
        seconds: seconds.unwrap_or(replay_seconds).min(replay_seconds),
        edit,
    })
}

/// Whether an edit flag written at `written` is still valid at `now`. A
/// flag from the future (the clock moved) counts as fresh.
pub fn edit_flag_is_fresh(written: SystemTime, now: SystemTime) -> bool {
    match now.duration_since(written) {
        Ok(age) => age < EDIT_FLAG_LIFETIME,
        Err(_) => true,
    }
}

/// The freedesktop sounds Montage plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// A recording started.
    RecordingStarted,
    /// A recording was saved.
    RecordingSaved,
    /// A replay was saved.
    ReplaySaved,
    Error,
}

impl Sound {
    pub fn name(self) -> &'static str {
        match self {
            Sound::RecordingStarted => "device-added",
            Sound::RecordingSaved => "device-removed",
            Sound::ReplaySaved => "complete",
            Sound::Error => "dialog-error",
        }
    }

    /// The sound's file in the freedesktop sound theme, looked up in
    /// `data_dirs` in order.
    pub fn find(self, data_dirs: &[PathBuf]) -> Option<PathBuf> {
        data_dirs
            .iter()
            .map(|dir| {
                dir.join("sounds/freedesktop/stereo")
                    .join(format!("{}.oga", self.name()))
            })
            .find(|path| path.is_file())
    }
}

/// `$XDG_DATA_HOME` then `$XDG_DATA_DIRS`, with the spec's defaults.
pub fn data_dirs(
    data_home: Option<OsString>,
    data_dirs: Option<OsString>,
    home: &Path,
) -> Vec<PathBuf> {
    let mut dirs = vec![match data_home.filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home.join(".local/share"),
    }];
    let system = data_dirs
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(std::env::split_paths(&system));
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todays_command_line() {
        let settings = RecorderSettings {
            monitor: "DP-5".into(),
            fps: 144,
            codec: Codec::Hevc,
            bitrate_kbps: 60_000,
            replay_seconds: 60,
            replay_storage: ReplayStorage::Ram,
            replay_dir: "/home/a/Videos/Replays".into(),
            record_dir: "/home/a/Videos/Recordings".into(),
            audio: vec!["app-inverse:gsr-chime".into(), "default_input".into()],
        };
        let args = settings.args(
            Path::new("/run/user/1000/montage/montage-on-save"),
            Path::new("/run/user/1000/gsr.sock"),
        );
        let line: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
        assert_eq!(
            line.join(" "),
            "-w DP-5 -f 144 -k hevc -bm cbr -q 60000 \
             -a app-inverse:gsr-chime -a default_input -c mp4 -r 60 -replay-storage ram \
             -df yes -o /home/a/Videos/Replays -ro /home/a/Videos/Recordings \
             -sc /run/user/1000/montage/montage-on-save -ipc /run/user/1000/gsr.sock"
        );
    }

    #[test]
    fn the_sounds_stay_out_of_desktop_tracks() {
        assert_eq!(without_chime("default_output"), "app-inverse:gsr-chime");
        assert_eq!(
            without_chime("app-inverse:gsr-chime"),
            "app-inverse:gsr-chime"
        );
        assert_eq!(
            without_chime("default_output|default_input"),
            "app-inverse:gsr-chime|default_input"
        );
        assert_eq!(
            without_chime("app-inverse:discord"),
            "app-inverse:discord|app-inverse:gsr-chime"
        );
        assert_eq!(without_chime("default_input"), "default_input");
        assert_eq!(without_chime("app:firefox"), "app:firefox");
    }

    #[test]
    fn save_arguments_come_in_either_order() {
        let ok = |args: &[&str]| parse_save_args(args, 60).unwrap();
        assert_eq!(
            ok(&[]),
            SaveRequest {
                seconds: 60,
                edit: false
            }
        );
        assert_eq!(
            ok(&["15"]),
            SaveRequest {
                seconds: 15,
                edit: false
            }
        );
        assert_eq!(
            ok(&["15", "edit"]),
            SaveRequest {
                seconds: 15,
                edit: true
            }
        );
        assert_eq!(
            ok(&["edit", "15"]),
            SaveRequest {
                seconds: 15,
                edit: true
            }
        );
        assert_eq!(
            ok(&["edit"]),
            SaveRequest {
                seconds: 60,
                edit: true
            }
        );
    }

    #[test]
    fn save_length_is_capped_at_the_buffer() {
        assert_eq!(parse_save_args(&["90"], 60).unwrap().seconds, 60);
        assert_eq!(
            parse_save_args(&["99999999999999999999"], 60)
                .unwrap()
                .seconds,
            60
        );
        assert_eq!(parse_save_args(&["90"], 120).unwrap().seconds, 90);
        assert_eq!(parse_save_args::<&str>(&[], 30).unwrap().seconds, 30);
    }

    #[test]
    fn save_refuses_what_isnt_seconds() {
        for bad in ["0", "015", "-5", "+5", "1.5", "abc", "", "15s", "Edit"] {
            assert_eq!(parse_save_args(&[bad], 60), Err(bad.to_string()), "{bad:?}");
        }
        assert_eq!(parse_save_args(&["15", "20"], 60), Err("20".to_string()));
    }

    #[test]
    fn edit_flag_lasts_ten_seconds() {
        let now = SystemTime::now();
        assert!(edit_flag_is_fresh(now - Duration::from_secs(9), now));
        assert!(!edit_flag_is_fresh(now - Duration::from_secs(10), now));
        assert!(edit_flag_is_fresh(now + Duration::from_secs(3), now));
    }

    #[test]
    fn sounds_are_found_in_data_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("a");
        let second = tmp.path().join("b");
        let file = second.join("sounds/freedesktop/stereo/complete.oga");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"").unwrap();
        assert_eq!(
            Sound::ReplaySaved.find(&[first.clone(), second.clone()]),
            Some(file)
        );
        assert_eq!(Sound::Error.find(&[first, second]), None);
    }

    #[test]
    #[cfg(unix)]
    fn data_dirs_follow_the_spec() {
        let home = Path::new("/home/a");
        assert_eq!(
            data_dirs(None, None, home),
            [
                PathBuf::from("/home/a/.local/share"),
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share")
            ]
        );
        assert_eq!(
            data_dirs(Some("/d".into()), Some("/x:/y".into()), home),
            [
                PathBuf::from("/d"),
                PathBuf::from("/x"),
                PathBuf::from("/y")
            ]
        );
    }

    #[test]
    fn buffer_size_is_estimated() {
        let s = RecorderSettings::default();
        assert_eq!(s.replay_megabytes(), 450.0);
    }
}
