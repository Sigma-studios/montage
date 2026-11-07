//! The library: every clip in the library folders, newest first, grouped by
//! day, with their sizes and badges. Reads only; starring adopts the clip
//! (its star lives in `montage.toml`), deleting is the binary's (trash).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::clip::{Clip, ClipError, VIDEO_EXTENSIONS};
use crate::edit::Edit;

#[derive(Debug, Clone)]
pub struct LibraryClip {
    pub clip: Clip,
    /// When the clip was recorded (the original's modification time).
    pub recorded: SystemTime,
    /// Bytes of the whole clip folder (original, exports, cache), or of the
    /// file for a loose clip.
    pub bytes: u64,
    pub has_edit: bool,
    pub has_export: bool,
    pub starred: bool,
}

#[derive(Debug, Clone)]
pub struct Day {
    /// `2026-11-07`: the recorder's day folder name, else the local date the
    /// clip was recorded.
    pub label: String,
    pub clips: Vec<LibraryClip>,
}

/// Every clip under `folders` (loose files and clip folders, at any depth up
/// to the day folders), newest first, grouped by day. Missing folders are
/// skipped.
pub fn scan(folders: &[PathBuf]) -> Vec<Day> {
    let mut found: Vec<(String, LibraryClip)> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for root in folders {
        if !root.is_dir() {
            continue;
        }
        // The folder itself may be a clip's folder.
        if let Ok(clip) = Clip::locate(root)
            && clip.in_folder
        {
            add(clip, &mut found, &mut seen);
            continue;
        }
        visit(root, 0, &mut found, &mut seen);
    }

    // Newest first, and days newest first. Labels are ISO dates, so they
    // sort as strings; a day's position follows its label, not its clips.
    found.sort_by(|(da, a), (db, b)| db.cmp(da).then(b.recorded.cmp(&a.recorded)));
    let mut days: Vec<Day> = Vec::new();
    for (label, clip) in found {
        match days.last_mut() {
            Some(day) if day.label == label => day.clips.push(clip),
            _ => days.push(Day {
                label,
                clips: vec![clip],
            }),
        }
    }
    days
}

/// Lists `dir`: loose videos and clip folders are clips; other folders are
/// day folders, entered when `dir` is a library folder (`depth` 0), so the
/// deepest file looked at is `root/day/clip/file`.
fn visit(
    dir: &Path,
    depth: usize,
    found: &mut Vec<(String, LibraryClip)>,
    seen: &mut HashSet<PathBuf>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        // `.thumbs` and other hidden files are never clips.
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // Follow symlinks for what they point at.
        let is_dir = if file_type.is_symlink() {
            path.is_dir()
        } else {
            file_type.is_dir()
        };
        if is_dir {
            match Clip::locate(&path) {
                Ok(clip) => add(clip, found, seen),
                Err(_) if depth == 0 => visit(&path, depth + 1, found, seen),
                Err(_) => {}
            }
        } else if is_video(&path) {
            // `dir` isn't a clip folder (it would have been taken as a
            // clip, not listed), so a video in it is a loose clip.
            add(
                Clip {
                    original: path,
                    in_folder: false,
                },
                found,
                seen,
            );
        }
    }
}

fn add(clip: Clip, found: &mut Vec<(String, LibraryClip)>, seen: &mut HashSet<PathBuf>) {
    let key = fs::canonicalize(&clip.original).unwrap_or_else(|_| clip.original.clone());
    if !seen.insert(key) {
        // Library folders that overlap list a clip once.
        return;
    }
    let Some(entry) = describe(clip) else {
        return;
    };
    let label = day_label(&entry);
    found.push((label, entry));
}

/// Reads what the library shows about a clip. `None` when its original
/// vanished meanwhile.
fn describe(clip: Clip) -> Option<LibraryClip> {
    let meta = fs::metadata(&clip.original).ok()?;
    let recorded = meta.modified().unwrap_or(UNIX_EPOCH);
    let bytes = if clip.in_folder {
        dir_size(&clip.folder())
    } else {
        meta.len()
    };

    let (has_edit, starred) = if clip.in_folder {
        match Edit::load(&clip.edit_path()) {
            Ok(Some(edit)) => (edit_changes_something(&edit), edit.starred),
            Ok(None) => (false, false),
            // An edit Montage can't read (newer, damaged) is still an edit.
            Err(_) => (true, false),
        }
    } else {
        (false, false)
    };
    let has_export = clip.in_folder && folder_has_export(&clip);

    Some(LibraryClip {
        clip,
        recorded,
        bytes,
        has_edit,
        has_export,
        starred,
    })
}

/// A cut, a track's volume or mute, or a caption: what makes an edit worth
/// a badge (a star alone isn't one).
fn edit_changes_something(edit: &Edit) -> bool {
    edit.cut.is_some()
        || !edit.captions.is_empty()
        || edit.tracks.iter().any(|t| t.muted || t.volume != 1.0)
}

/// Whether the clip's folder holds one of its exports: `<name>-share….mp4`,
/// `<name>-edit….mp4` or `<name>….gif`.
fn folder_has_export(clip: &Clip) -> bool {
    let name = clip.name().to_string_lossy().into_owned();
    let Ok(entries) = fs::read_dir(clip.folder()) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        let (Some(stem), Some(ext)) = (path.file_stem(), path.extension()) else {
            return false;
        };
        let stem = stem.to_string_lossy();
        let Some(rest) = stem.strip_prefix(name.as_str()) else {
            return false;
        };
        let ext = ext.to_string_lossy().to_ascii_lowercase();
        match ext.as_str() {
            "mp4" => rest.starts_with("-share") || rest.starts_with("-edit"),
            "gif" => rest.is_empty() || rest.starts_with('-'),
            _ => false,
        }
    })
}

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| VIDEO_EXTENSIONS.iter().any(|v| v.eq_ignore_ascii_case(ext)))
}

/// Bytes of every file under `dir`, without following links.
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(t) if t.is_dir() => dir_size(&entry.path()),
            Ok(t) if t.is_file() => entry.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// The day a clip is listed under: the name of the folder it sits in when
/// that's a date (gpu-screen-recorder's day folders), else a date in the
/// clip's name (recorders name files after the local time they started,
/// `Replay_2026-11-07_21-03-11`, OBS's `2026-11-07 21-03-11`), else the
/// date of the original's modification time.
fn day_label(entry: &LibraryClip) -> String {
    let clip = &entry.clip;
    let holder = if clip.in_folder {
        clip.folder().parent().map(Path::to_path_buf)
    } else {
        clip.original.parent().map(Path::to_path_buf)
    };
    if let Some(name) = holder.as_deref().and_then(Path::file_name)
        && let Some(name) = name.to_str()
        && is_iso_date(name)
    {
        return name.to_string();
    }
    if let Some(date) = find_iso_date(&clip.name().to_string_lossy()) {
        return date;
    }
    date_of(entry.recorded)
}

/// `YYYY-MM-DD` with a plausible month and day.
fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if !(digits(0..4) && digits(5..7) && digits(8..10)) {
        return false;
    }
    let month: u32 = s[5..7].parse().unwrap_or(0);
    let day: u32 = s[8..10].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// The first `YYYY-MM-DD` in `s` not glued to other digits.
fn find_iso_date(s: &str) -> Option<String> {
    let b = s.as_bytes();
    (0..b.len().saturating_sub(9)).find_map(|i| {
        let before_ok = i == 0 || !b[i - 1].is_ascii_digit();
        let after_ok = b.get(i + 10).is_none_or(|c| !c.is_ascii_digit());
        let candidate = s.get(i..i + 10)?;
        (before_ok && after_ok && is_iso_date(candidate)).then(|| candidate.to_string())
    })
}

/// The date of `time`, in UTC. The local date would need the time zone
/// database (a dependency, or unsafe calls into libc, which this crate
/// forbids); the day folders and the dates in clip names cover nearly every
/// clip, so this only places clips with neither, and only those recorded
/// near midnight can land on the neighbouring day.
fn date_of(time: SystemTime) -> String {
    let secs = match time.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a
/// proleptic Gregorian (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Total bytes of every clip listed.
pub fn total_bytes(days: &[Day]) -> u64 {
    days.iter().flat_map(|d| &d.clips).map(|c| c.bytes).sum()
}

/// Stars or unstars a clip, adopting it into its folder first. Returns the
/// clip as it now is.
pub fn set_starred(clip: &Clip, starred: bool) -> Result<Clip, ClipError> {
    let edit_err = |path: &Path| {
        let path = path.to_path_buf();
        move |e: crate::edit::EditError| ClipError::Io {
            action: "can't star",
            path,
            source: io::Error::other(e),
        }
    };
    // A loose clip has no edit, so it isn't starred: nothing to unstar, and
    // no reason to move it.
    if !starred && !clip.in_folder {
        return Ok(clip.clone());
    }
    let clip = clip.adopt()?;
    let path = clip.edit_path();
    let existing = Edit::load(&path).map_err(edit_err(&path))?;
    if existing.as_ref().is_some_and(|e| e.starred) == starred {
        return Ok(clip);
    }
    let mut edit = existing.unwrap_or_default();
    edit.starred = starred;
    edit.save(&path).map_err(edit_err(&path))?;
    Ok(clip)
}

/// What to send to the trash to delete a clip: its folder, or the file of a
/// loose clip.
pub fn deletion_target(clip: &Clip) -> &Path {
    if clip.in_folder {
        clip.original.parent().unwrap_or(&clip.original)
    } else {
        &clip.original
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::{Caption, Cut, TrackSettings};
    use std::time::Duration;

    fn write(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![0u8; bytes]).unwrap();
    }

    /// Sets the modification time, so ordering doesn't depend on how fast
    /// the test writes.
    fn age(path: &Path, secs_since_epoch: u64) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(secs_since_epoch))
            .unwrap();
    }

    fn clip_folder(day: &Path, name: &str, bytes: usize, mtime: u64) -> PathBuf {
        let folder = day.join(name);
        let original = folder.join(format!("{name}.mp4"));
        write(&original, bytes);
        age(&original, mtime);
        folder
    }

    fn names(days: &[Day]) -> Vec<(String, Vec<String>)> {
        days.iter()
            .map(|d| {
                (
                    d.label.clone(),
                    d.clips
                        .iter()
                        .map(|c| c.clip.name().to_string_lossy().into_owned())
                        .collect(),
                )
            })
            .collect()
    }

    fn find<'a>(days: &'a [Day], name: &str) -> &'a LibraryClip {
        days.iter()
            .flat_map(|d| &d.clips)
            .find(|c| c.clip.name() == name)
            .unwrap_or_else(|| panic!("{name} not listed"))
    }

    // 2026-11-07 12:00 UTC.
    const NOV7: u64 = 1_794_052_800;

    #[test]
    fn scan_finds_clip_folders_and_loose_files_grouped_by_day() {
        let tmp = tempfile::tempdir().unwrap();
        let replays = tmp.path().join("Replays");
        let recordings = tmp.path().join("Recordings");
        let d7 = replays.join("2026-11-07");
        let d6 = replays.join("2026-11-06");

        let a = clip_folder(&d7, "Replay_2026-11-07_21-03-11", 1000, NOV7 + 9 * 3600);
        write(&a.join("Replay_2026-11-07_21-03-11-share.mp4"), 300);
        write(&a.join(".thumbs").join("thumb-180.png"), 50);
        clip_folder(&d7, "Replay_2026-11-07_20-00-00", 10, NOV7 + 8 * 3600);
        let loose = d7.join("Replay_2026-11-07_22-00-00.mkv");
        write(&loose, 70);
        age(&loose, NOV7 + 10 * 3600);
        // Not clips: a text file, a folder without its original, a hidden
        // folder, a cache folder at the day level.
        write(&d7.join("notes.txt"), 5);
        write(&d7.join("stray").join("other.mp4"), 5);
        write(&d7.join(".hidden").join(".hidden.mp4"), 5);
        write(&d7.join(".thumbs").join("x.mp4"), 5);

        let b = clip_folder(&d6, "Replay_2026-11-06_23-59-00", 20, NOV7 - 3600);
        write(&b.join("Replay_2026-11-06_23-59-00.gif"), 5);

        // A recording straight in its library folder: dated from its name.
        let rec = recordings.join("Video_2026-11-05_10-00-00.mp4");
        write(&rec, 40);
        age(&rec, NOV7);

        let days = scan(&[
            replays.clone(),
            recordings.clone(),
            tmp.path().join("missing"),
        ]);
        assert_eq!(
            names(&days),
            [
                (
                    "2026-11-07".to_string(),
                    vec![
                        "Replay_2026-11-07_22-00-00".to_string(),
                        "Replay_2026-11-07_21-03-11".to_string(),
                        "Replay_2026-11-07_20-00-00".to_string(),
                    ]
                ),
                (
                    "2026-11-06".to_string(),
                    vec!["Replay_2026-11-06_23-59-00".to_string()]
                ),
                (
                    "2026-11-05".to_string(),
                    vec!["Video_2026-11-05_10-00-00".to_string()]
                ),
            ]
        );

        let a = find(&days, "Replay_2026-11-07_21-03-11");
        assert!(a.clip.in_folder);
        assert_eq!(a.bytes, 1000 + 300 + 50, "the whole folder, cache included");
        assert!(a.has_export);
        assert!(!a.has_edit);
        assert!(!a.starred);

        let l = find(&days, "Replay_2026-11-07_22-00-00");
        assert!(!l.clip.in_folder);
        assert_eq!(l.bytes, 70);
        assert!(!l.has_export && !l.has_edit);

        assert!(
            find(&days, "Replay_2026-11-06_23-59-00").has_export,
            "a GIF"
        );
        assert!(!find(&days, "Replay_2026-11-07_20-00-00").has_export);
        assert_eq!(total_bytes(&days), 1350 + 10 + 70 + 25 + 40);
    }

    #[test]
    fn undated_clips_take_the_date_they_were_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Movies");
        let file = root.join("gameplay.mov");
        write(&file, 1);
        age(&file, NOV7);
        let other = root.join("Stuff").join("older.mp4");
        write(&other, 1);
        age(&other, NOV7 - 3 * 86_400);
        // Too deep: root/day/x/y/file.
        write(&root.join("a").join("b").join("c").join("deep.mp4"), 1);

        let days = scan(&[root]);
        assert_eq!(
            names(&days),
            [
                ("2026-11-07".to_string(), vec!["gameplay".to_string()]),
                ("2026-11-04".to_string(), vec!["older".to_string()]),
            ]
        );
    }

    #[test]
    fn overlapping_folders_list_a_clip_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Videos");
        let day = root.join("2026-11-07");
        clip_folder(&day, "c", 1, NOV7);
        let days = scan(&[root.clone(), day.clone(), day.join("c")]);
        assert_eq!(days.iter().map(|d| d.clips.len()).sum::<usize>(), 1);
    }

    #[test]
    fn edits_are_badged_only_when_they_change_something() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("2026-11-07");
        let cut = clip_folder(&day, "cut", 1, NOV7);
        let muted = clip_folder(&day, "muted", 1, NOV7);
        let caption = clip_folder(&day, "caption", 1, NOV7);
        let star = clip_folder(&day, "star", 1, NOV7);
        let broken = clip_folder(&day, "broken", 1, NOV7);

        Edit {
            cut: Some(Cut {
                start: 1.0,
                end: 2.0,
            }),
            ..Edit::default()
        }
        .save(&cut.join("montage.toml"))
        .unwrap();
        let mut e = Edit::default();
        e.set_track(TrackSettings {
            index: 0,
            muted: true,
            volume: 1.0,
        });
        e.save(&muted.join("montage.toml")).unwrap();
        Edit {
            captions: vec![Caption::default()],
            ..Edit::default()
        }
        .save(&caption.join("montage.toml"))
        .unwrap();
        Edit {
            starred: true,
            ..Edit::default()
        }
        .save(&star.join("montage.toml"))
        .unwrap();
        fs::write(broken.join("montage.toml"), "version = 99\n").unwrap();

        let days = scan(&[tmp.path().to_path_buf()]);
        for name in ["cut", "muted", "caption", "broken"] {
            assert!(find(&days, name).has_edit, "{name}");
        }
        let s = find(&days, "star");
        assert!(!s.has_edit);
        assert!(s.starred);
        assert!(!find(&days, "cut").starred);
    }

    #[test]
    fn starring_adopts_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("2026-11-07");
        let file = day.join("Replay_x.mp4");
        write(&file, 3);
        let loose = Clip::locate(&file).unwrap();

        // Unstarring a loose clip moves nothing.
        assert_eq!(set_starred(&loose, false).unwrap(), loose);
        assert!(file.exists());

        let starred = set_starred(&loose, true).unwrap();
        assert!(starred.in_folder);
        assert_eq!(starred.original, day.join("Replay_x").join("Replay_x.mp4"));
        assert!(!file.exists());
        let days = scan(&[tmp.path().to_path_buf()]);
        assert!(find(&days, "Replay_x").starred);

        // The rest of the edit survives a star.
        let mut edit = Edit::load(&starred.edit_path()).unwrap().unwrap();
        edit.cut = Some(Cut {
            start: 0.0,
            end: 1.0,
        });
        edit.save(&starred.edit_path()).unwrap();

        let unstarred = set_starred(&starred, false).unwrap();
        assert_eq!(unstarred, starred);
        let edit = Edit::load(&starred.edit_path()).unwrap().unwrap();
        assert!(!edit.starred);
        assert!(edit.cut.is_some());
        let days = scan(&[tmp.path().to_path_buf()]);
        let c = find(&days, "Replay_x");
        assert!(!c.starred);
        assert!(c.has_edit);
    }

    #[test]
    fn deletion_takes_the_folder_or_the_loose_file() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = clip_folder(tmp.path(), "a", 1, NOV7);
        let in_folder = Clip::locate(&folder).unwrap();
        assert_eq!(deletion_target(&in_folder), folder);

        let file = tmp.path().join("b.mp4");
        write(&file, 1);
        let loose = Clip::locate(&file).unwrap();
        assert_eq!(deletion_target(&loose), file);
    }

    #[test]
    fn dates() {
        assert_eq!(date_of(UNIX_EPOCH), "1970-01-01");
        assert_eq!(
            date_of(UNIX_EPOCH + Duration::from_secs(NOV7)),
            "2026-11-07"
        );
        assert_eq!(
            date_of(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29"
        );
        assert_eq!(date_of(UNIX_EPOCH - Duration::from_secs(1)), "1969-12-31");
        assert!(is_iso_date("2026-11-07"));
        assert!(!is_iso_date("2026-13-07"));
        assert!(!is_iso_date("2026-11-7x"));
        assert!(!is_iso_date("Replays"));
        assert_eq!(
            find_iso_date("Replay_2026-11-07_21-03-11").as_deref(),
            Some("2026-11-07")
        );
        assert_eq!(
            find_iso_date("2026-11-07 21-03-11").as_deref(),
            Some("2026-11-07")
        );
        assert_eq!(find_iso_date("12026-11-07"), None);
        assert_eq!(find_iso_date("clip"), None);
    }
}
