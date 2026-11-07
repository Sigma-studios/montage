//! Applying recorder settings: the service is restarted, which empties the
//! replay buffer and ends a recording, so it can wait for a recording to
//! stop first. Linux only at run time.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::Duration;

use montage_core::recorder::RECORDING_FLAG;

/// The user service running the recorder (see `nix/hm-module.nix`).
pub const SERVICE: &str = "gpu-screen-recorder";

/// What the restart thread reports.
pub enum Progress {
    /// Waiting for the recording to stop.
    Waiting,
    Restarting,
    Done(Result<(), String>),
}

/// The flag that exists while a recording runs, when there's a runtime
/// folder.
pub fn recording_flag() -> Option<PathBuf> {
    env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(|dir| PathBuf::from(dir).join(RECORDING_FLAG))
}

pub fn recording_running() -> bool {
    recording_flag().is_some_and(|f| f.exists())
}

/// `systemctl --user restart gpu-screen-recorder`.
pub fn restart_service() -> Result<(), String> {
    let output = Command::new("systemctl")
        .args(["--user", "restart", SERVICE])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("can't run systemctl: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("systemctl failed ({})", output.status)
    } else {
        stderr
    })
}

/// Runs `restart` on a thread, after `flag` disappears when given. Each
/// step is sent to `progress`, then `wake` is called.
pub fn restart_later(
    flag: Option<PathBuf>,
    restart: impl FnOnce() -> Result<(), String> + Send + 'static,
    progress: Sender<Progress>,
    wake: impl Fn() + Send + 'static,
) {
    std::thread::spawn(move || {
        if let Some(flag) = flag
            && flag.exists()
        {
            let _ = progress.send(Progress::Waiting);
            wake();
            while flag.exists() {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        let _ = progress.send(Progress::Restarting);
        wake();
        let _ = progress.send(Progress::Done(restart()));
        wake();
    });
}

/// Whether a folder can be used for writing clips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderCheck {
    Writable,
    /// Doesn't exist, but can be created (the recorder creates it).
    WillBeCreated,
    Problem(String),
}

/// Checks that `folder`, or the nearest folder above it that exists, can be
/// written to, by creating and removing a file. Creates no folder.
pub fn check_folder(folder: &Path) -> FolderCheck {
    if folder.as_os_str().is_empty() {
        return FolderCheck::Problem("no folder chosen".into());
    }
    if !folder.is_absolute() {
        return FolderCheck::Problem("give a full path".into());
    }
    match fs::metadata(folder) {
        Ok(meta) if meta.is_dir() => match try_write(folder) {
            Ok(()) => FolderCheck::Writable,
            Err(e) => FolderCheck::Problem(format!("can't write here: {e}")),
        },
        Ok(_) => FolderCheck::Problem("this is a file, not a folder".into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let Some(existing) = folder.ancestors().skip(1).find(|p| p.is_dir()) else {
                return FolderCheck::Problem("no existing folder above it".into());
            };
            match try_write(existing) {
                Ok(()) => FolderCheck::WillBeCreated,
                Err(e) => {
                    FolderCheck::Problem(format!("can't be created in {}: {e}", existing.display()))
                }
            }
        }
        Err(e) => FolderCheck::Problem(e.to_string()),
    }
}

fn try_write(folder: &Path) -> io::Result<()> {
    let probe = folder.join(format!(".montage-write-test-{}", std::process::id()));
    fs::write(&probe, b"")?;
    fs::remove_file(&probe)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_problem(folder: &Path) -> bool {
        matches!(check_folder(folder), FolderCheck::Problem(_))
    }

    #[test]
    fn folders_are_checked_without_being_created() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(check_folder(tmp.path()), FolderCheck::Writable);
        let new = tmp.path().join("a").join("b");
        assert_eq!(check_folder(&new), FolderCheck::WillBeCreated);
        assert!(!tmp.path().join("a").exists());

        let file = tmp.path().join("file");
        fs::write(&file, b"").unwrap();
        assert!(is_problem(&file));
        assert!(is_problem(&file.join("below")));
        assert!(is_problem(Path::new("")));
        assert!(is_problem(Path::new("relative/path")));
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1, "no leftovers");
    }

    #[cfg(unix)]
    #[test]
    fn read_only_folders_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let ro = tmp.path().join("ro");
        fs::create_dir(&ro).unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
        let writable_anyway = try_write(&ro).is_ok(); // running as root
        if !writable_anyway {
            assert!(is_problem(&ro));
            assert!(is_problem(&ro.join("new")));
        }
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn restart_waits_for_the_recording() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let tmp = tempfile::tempdir().unwrap();
        let flag = tmp.path().join(RECORDING_FLAG);
        fs::write(&flag, b"").unwrap();
        let restarted = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let r = restarted.clone();
        let restart = move || {
            r.store(true, Ordering::SeqCst);
            Err("no systemd here".to_string())
        };
        restart_later(Some(flag.clone()), restart, tx, || {});
        let next = || rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(next(), Progress::Waiting));
        std::thread::sleep(Duration::from_millis(700));
        assert!(!restarted.load(Ordering::SeqCst), "still waiting");
        fs::remove_file(&flag).unwrap();
        assert!(matches!(next(), Progress::Restarting));
        assert!(matches!(next(), Progress::Done(Err(e)) if e == "no systemd here"));
        assert!(restarted.load(Ordering::SeqCst));
    }
}
