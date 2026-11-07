//! One folder per clip.
//!
//! A clip is an original video file. Once adopted, it lives in a folder named
//! after it, next to its edit (`montage.toml`), its exports and its thumbnail
//! cache. Clips that are still loose files are adopted the first time they're
//! opened or exported; nothing is moved before that.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Extensions recognised as a clip's original.
pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "mov", "webm", "flv", "m4v"];

/// The file holding a clip's edit.
pub const EDIT_FILE: &str = "montage.toml";

/// The folder holding a clip's filmstrip and waveform cache.
pub const THUMBS_DIR: &str = ".thumbs";

#[derive(Debug, thiserror::Error)]
pub enum ClipError {
    #[error("{0} is not a video file")]
    NotAVideo(PathBuf),
    #[error("{0} holds no clip: no video named after the folder")]
    NoOriginal(PathBuf),
    #[error("{path} is an export of the clip {original}, not a clip")]
    IsExport { path: PathBuf, original: PathBuf },
    #[error("can't move {file} into {folder}: {folder} already holds a file with that name")]
    FolderTaken { file: PathBuf, folder: PathBuf },
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

fn io_err(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> ClipError {
    let path = path.to_path_buf();
    move |source| ClipError::Io {
        action,
        path,
        source,
    }
}

/// The kinds of export, each with its own file name pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    /// The automatic copy made by `montage share`: `<name>-share.mp4`.
    Share,
    /// An MP4 exported from the editor: `<name>-edit.mp4`.
    Edit,
    /// A GIF: `<name>.gif`.
    Gif,
}

impl ExportKind {
    fn suffix_and_extension(self) -> (&'static str, &'static str) {
        match self {
            ExportKind::Share => ("-share", "mp4"),
            ExportKind::Edit => ("-edit", "mp4"),
            ExportKind::Gif => ("", "gif"),
        }
    }
}

/// A clip, located on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    /// The original video file.
    pub original: PathBuf,
    /// Whether the original already sits in a folder of its own.
    pub in_folder: bool,
}

impl Clip {
    /// Finds the clip that `path` names: the original file (loose, or inside
    /// its folder) or the clip's folder. Moves nothing.
    pub fn locate(path: &Path) -> Result<Clip, ClipError> {
        let meta = fs::metadata(path).map_err(io_err("can't read", path))?;
        if meta.is_dir() {
            let original =
                find_original(path)?.ok_or_else(|| ClipError::NoOriginal(path.into()))?;
            return Ok(Clip {
                original,
                in_folder: true,
            });
        }

        if !is_video(path) {
            return Err(ClipError::NotAVideo(path.into()));
        }
        let stem = path.file_stem().unwrap_or_default();
        let parent = path.parent().unwrap_or(Path::new(""));
        let parent_name = parent.file_name().unwrap_or_default();
        if parent_name == stem {
            return Ok(Clip {
                original: path.into(),
                in_folder: true,
            });
        }

        // A file in a clip folder that isn't named after the folder is one of
        // its exports. Adopting it would bury it in a folder of its own.
        if let Some(original) = find_original(parent)?
            && stem
                .to_string_lossy()
                .starts_with(&*parent_name.to_string_lossy())
        {
            return Err(ClipError::IsExport {
                path: path.into(),
                original,
            });
        }

        Ok(Clip {
            original: path.into(),
            in_folder: false,
        })
    }

    /// The clip's name: the original's file name without its extension.
    pub fn name(&self) -> &OsStr {
        self.original.file_stem().unwrap_or_default()
    }

    /// The folder the clip lives in. For a loose clip, the folder it will be
    /// moved into once adopted.
    pub fn folder(&self) -> PathBuf {
        let parent = self.original.parent().unwrap_or(Path::new(""));
        if self.in_folder {
            parent.to_path_buf()
        } else {
            parent.join(self.name())
        }
    }

    pub fn edit_path(&self) -> PathBuf {
        self.folder().join(EDIT_FILE)
    }

    pub fn thumbs_dir(&self) -> PathBuf {
        self.folder().join(THUMBS_DIR)
    }

    /// Moves a loose clip into a folder of its own named after it. Does
    /// nothing for a clip already in its folder.
    pub fn adopt(&self) -> Result<Clip, ClipError> {
        if self.in_folder {
            return Ok(self.clone());
        }
        let folder = self.folder();
        let target = folder.join(self.original.file_name().unwrap_or_default());
        if target.exists() {
            return Err(ClipError::FolderTaken {
                file: self.original.clone(),
                folder,
            });
        }
        fs::create_dir_all(&folder).map_err(io_err("can't create", &folder))?;
        rename_when_free(&self.original, &target).map_err(io_err("can't move", &self.original))?;
        Ok(Clip {
            original: target,
            in_folder: true,
        })
    }

    /// The path for a new export of `kind`, in the clip's folder: the first
    /// of `<name>-edit.mp4`, `<name>-edit-2.mp4`, … that doesn't exist yet.
    pub fn next_export_path(&self, kind: ExportKind) -> PathBuf {
        let folder = self.folder();
        let name = self.name().to_string_lossy();
        let (suffix, ext) = kind.suffix_and_extension();
        (1..)
            .map(|n| match n {
                1 => folder.join(format!("{name}{suffix}.{ext}")),
                n => folder.join(format!("{name}{suffix}-{n}.{ext}")),
            })
            .find(|p| !p.exists())
            .expect("an unbounded range always has a free name")
    }
}

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| VIDEO_EXTENSIONS.iter().any(|v| v.eq_ignore_ascii_case(ext)))
}

/// The video in `folder` named after the folder, if there is one.
fn find_original(folder: &Path) -> Result<Option<PathBuf>, ClipError> {
    let Some(name) = folder.file_name() else {
        return Ok(None);
    };
    let entries = match fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err("can't list", folder)(e)),
    };
    for entry in entries {
        let path = entry.map_err(io_err("can't list", folder))?.path();
        if path.is_file() && is_video(&path) && path.file_stem() == Some(name) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Renames `from` to `to`. On Windows, a file the recorder hasn't closed yet
/// can't be moved, so this retries for a few seconds on a sharing violation.
fn rename_when_free(from: &Path, to: &Path) -> io::Result<()> {
    const PATIENCE: Duration = Duration::from_secs(5);
    let started = Instant::now();
    loop {
        match fs::rename(from, to) {
            Err(e) if is_sharing_violation(&e) && started.elapsed() < PATIENCE => {
                std::thread::sleep(Duration::from_millis(200));
            }
            result => return result,
        }
    }
}

fn is_sharing_violation(e: &io::Error) -> bool {
    // ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION.
    cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn loose_clip_is_adopted_into_its_own_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("2026-11-07");
        let file = day.join("Replay_2026-11-07_21-03-11.mp4");
        touch(&file);

        let clip = Clip::locate(&file).unwrap();
        assert!(!clip.in_folder);
        assert_eq!(clip.folder(), day.join("Replay_2026-11-07_21-03-11"));
        assert!(file.exists(), "locating moves nothing");

        let adopted = clip.adopt().unwrap();
        let expected = day
            .join("Replay_2026-11-07_21-03-11")
            .join("Replay_2026-11-07_21-03-11.mp4");
        assert_eq!(adopted.original, expected);
        assert!(adopted.in_folder);
        assert!(expected.exists());
        assert!(!file.exists());
        assert_eq!(adopted.edit_path(), adopted.folder().join("montage.toml"));
    }

    #[test]
    fn adopting_twice_is_harmless() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("clip.mkv");
        touch(&file);
        let once = Clip::locate(&file).unwrap().adopt().unwrap();
        let twice = Clip::locate(&once.original).unwrap().adopt().unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn a_folder_names_its_clip() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("Replay_x");
        touch(&folder.join("Replay_x.mp4"));
        touch(&folder.join("Replay_x-share.mp4"));
        let clip = Clip::locate(&folder).unwrap();
        assert_eq!(clip.original, folder.join("Replay_x.mp4"));
        assert!(clip.in_folder);
    }

    #[test]
    fn a_folder_without_its_original_is_not_a_clip() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("Replay_x");
        touch(&folder.join("other.mp4"));
        assert!(matches!(
            Clip::locate(&folder),
            Err(ClipError::NoOriginal(_))
        ));
    }

    #[test]
    fn an_export_is_not_a_clip() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("Replay_x");
        touch(&folder.join("Replay_x.mp4"));
        let export = folder.join("Replay_x-edit.mp4");
        touch(&export);
        assert!(matches!(
            Clip::locate(&export),
            Err(ClipError::IsExport { .. })
        ));
    }

    #[test]
    fn other_files_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("notes.txt");
        touch(&file);
        assert!(matches!(Clip::locate(&file), Err(ClipError::NotAVideo(_))));
    }

    #[test]
    fn adopting_never_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("clip.mp4");
        touch(&file);
        touch(&tmp.path().join("clip").join("clip.mp4"));
        // `clip/clip.mp4` already exists: the loose file must be refused
        // rather than replace it.
        let clip = Clip::locate(&file).unwrap();
        assert!(matches!(clip.adopt(), Err(ClipError::FolderTaken { .. })));
        assert!(file.exists());
    }

    #[test]
    fn exports_are_numbered() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("Replay_x");
        touch(&folder.join("Replay_x.mp4"));
        let clip = Clip::locate(&folder).unwrap();

        assert_eq!(
            clip.next_export_path(ExportKind::Edit),
            folder.join("Replay_x-edit.mp4")
        );
        touch(&folder.join("Replay_x-edit.mp4"));
        assert_eq!(
            clip.next_export_path(ExportKind::Edit),
            folder.join("Replay_x-edit-2.mp4")
        );
        touch(&folder.join("Replay_x-edit-2.mp4"));
        assert_eq!(
            clip.next_export_path(ExportKind::Edit),
            folder.join("Replay_x-edit-3.mp4")
        );

        assert_eq!(
            clip.next_export_path(ExportKind::Share),
            folder.join("Replay_x-share.mp4")
        );
        assert_eq!(
            clip.next_export_path(ExportKind::Gif),
            folder.join("Replay_x.gif")
        );
        touch(&folder.join("Replay_x.gif"));
        assert_eq!(
            clip.next_export_path(ExportKind::Gif),
            folder.join("Replay_x-2.gif")
        );
    }

    #[test]
    fn extensions_are_case_insensitive() {
        assert!(is_video(Path::new("a/Clip.MP4")));
        assert!(is_video(Path::new("a/Clip.mov")));
        assert!(!is_video(Path::new("a/Clip.gif")));
    }
}
