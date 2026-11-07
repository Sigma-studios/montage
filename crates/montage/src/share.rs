//! `montage share`: the clip's Share export to the clipboard, with a
//! notification. What the recorder's hook runs for every new clip. Also
//! `montage export`, the same with any preset.

use std::path::{Path, PathBuf};

use anyhow::Result;
use montage_core::Config;
use montage_core::share::ClipKind;

/// The export presets, as `montage export --preset` names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// H.264 + AAC under the `[share]` budget.
    Share,
    /// The original's size and frame rate: the stream copied when it can
    /// be, else the GPU encoder.
    Full,
    /// The `[gif]` settings.
    Gif,
}

/// Exports the clip at `path` with `preset` and the clip's saved edit (cut,
/// track volumes, captions), into the clip's folder or to `out`. `kind`
/// says which end Share keeps when the clip is too long for the budget.
/// Returns the export's path and a one-line description for the
/// notification.
#[cfg(feature = "media")]
pub fn export(
    config: &Config,
    path: &Path,
    preset: Preset,
    kind: Option<ClipKind>,
    out: Option<&Path>,
) -> Result<(PathBuf, String)> {
    use std::sync::atomic::AtomicBool;

    use montage_core::render::Overlay;
    use montage_core::{Clip, Edit, ExportKind, share};
    use montage_media::{ExportJob, ExportPreset};

    let clip = Clip::locate(path)?.adopt()?;
    let edit = Edit::load(&clip.edit_path())?.unwrap_or_default();
    let info = montage_media::probe(&clip.original)?;
    let cut = edit.cut_within(info.duration);
    let gains: Vec<f32> = (0..info.audio_tracks)
        .map(|i| edit.track(i).gain())
        .collect();

    let kind = kind.unwrap_or_else(|| ClipKind::guess(&clip.name().to_string_lossy()));
    let plan = share::plan(&config.share, info.source(), kind, edit.cut);
    let (export_preset, export_kind) = match preset {
        Preset::Share => (
            ExportPreset::Share {
                plan,
                max_bytes: config.share.max_bytes,
            },
            ExportKind::Share,
        ),
        Preset::Full => (ExportPreset::Full, ExportKind::Edit),
        Preset::Gif => (ExportPreset::Gif(config.gif), ExportKind::Gif),
    };
    let output = match out {
        Some(out) => out.to_path_buf(),
        None => clip.next_export_path(export_kind),
    };

    // Loading the fonts takes a moment, so only for captions.
    let mut captions = (!edit.captions.is_empty()).then(|| {
        let fonts = std::sync::Arc::new(std::sync::Mutex::new(montage_text::Fonts::load()));
        montage_text::CaptionOverlay::new(fonts, edit.captions.clone(), cut)
    });
    let report = montage_media::export(
        ExportJob {
            input: &clip.original,
            output: &output,
            preset: export_preset,
            cut,
            gains: &gains,
            overlay: captions.as_mut().map(|c| c as &mut dyn Overlay),
        },
        &mut |_| {},
        &AtomicBool::new(false),
    )?;

    let name = output.file_name().unwrap_or_default().to_string_lossy();
    let mut description = format!("{name} · {}", megabytes(report.bytes));
    if preset == Preset::Share && plan.trimmed(cut) > 0.5 {
        let kept = match kind {
            ClipKind::Replay => "last",
            ClipKind::Regular => "first",
        };
        description.push_str(&format!(" · {kept} {:.0} s kept", plan.range.duration()));
    }
    Ok((output, description))
}

/// `montage share`: the Share export, then the clipboard and the
/// notification, whose "Edit" action opens the clip in the editor.
pub fn run(config: &Config, path: &Path, kind: Option<ClipKind>) -> Result<()> {
    let (output, description) = export(config, path, Preset::Share, kind, None)?;
    let clip = montage_core::Clip::locate(path)
        .map(|c| c.original)
        .unwrap_or_else(|_| output.clone());
    copy(&output, &description, Some(&clip))
}

/// Puts `output` on the clipboard and says so. On Linux, keeps serving the
/// clipboard until something else is copied. With `edit`, the notification
/// has an "Edit" action that opens that clip.
pub fn copy(output: &Path, description: &str, edit: Option<&Path>) -> Result<()> {
    use std::io::IsTerminal;

    crate::platform::clipboard::copy_file(output, || {
        match edit {
            Some(clip) => notify_with_edit(clip, description),
            None => crate::platform::notify("Clip copied", description),
        }
        if std::io::stderr().is_terminal() && cfg!(target_os = "linux") {
            eprintln!("montage: serving the clipboard until something else is copied");
        }
    })
}

/// The "Clip copied" notification with its "Edit" action. Waiting for the
/// click must not hold up the command: on Linux it waits on a thread of
/// this process, which stays alive serving the clipboard; on Windows and
/// macOS, where `share` returns at once, a detached copy of Montage waits.
fn notify_with_edit(clip: &Path, description: &str) {
    if cfg!(target_os = "linux") {
        let clip = clip.to_path_buf();
        let description = description.to_string();
        std::thread::spawn(move || {
            crate::platform::notify::notify_with_edit("Clip copied", &description, move || {
                if let Err(e) = open_detached(&clip) {
                    eprintln!("montage: {e:#}");
                }
            });
        });
    } else if let Err(e) = spawn_self(&[
        "notify-edit".as_ref(),
        clip.as_os_str(),
        "--description".as_ref(),
        description.as_ref(),
    ]) {
        eprintln!("montage: {e:#}; notifying without the Edit action");
        crate::platform::notify("Clip copied", description);
    }
}

/// `montage notify-edit`: shows the notification and waits for the click.
pub fn wait_for_edit(clip: &Path, description: &str) -> Result<()> {
    let clip = clip.to_path_buf();
    crate::platform::notify::notify_with_edit("Clip copied", description, move || {
        if let Err(e) = open_detached(&clip) {
            eprintln!("montage: {e:#}");
        }
    });
    Ok(())
}

/// Starts `montage open <clip>` so that it outlives this process. On Linux,
/// `share` runs in a systemd unit of its own, which stops whatever it left
/// running once it exits, so the editor gets a unit of its own too.
fn open_detached(clip: &Path) -> Result<()> {
    if cfg!(target_os = "linux") {
        let programs = crate::controls::Programs::from_env();
        let exe = std::env::current_exe()?;
        let started = std::process::Command::new(&programs.systemd_run)
            .args(["--user", "--collect", "--quiet", "--"])
            .arg(exe)
            .arg("open")
            .arg(clip)
            .stdin(std::process::Stdio::null())
            .status();
        if matches!(started, Ok(status) if status.success()) {
            return Ok(());
        }
    }
    spawn_self(&["open".as_ref(), clip.as_os_str()])
}

/// Starts Montage with `args`, in a process group of its own, without
/// waiting for it.
fn spawn_self(args: &[&std::ffi::OsStr]) -> Result<()> {
    use anyhow::Context;

    let exe = std::env::current_exe().context("can't find Montage's own path")?;
    let mut command = std::process::Command::new(exe);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command
        .spawn()
        .context("can't start another Montage process")?;
    Ok(())
}

#[cfg(not(feature = "media"))]
pub fn export(
    _config: &Config,
    path: &Path,
    _preset: Preset,
    _kind: Option<ClipKind>,
    _out: Option<&Path>,
) -> Result<(PathBuf, String)> {
    anyhow::bail!(
        "this build has no libav, so it can't export {}",
        path.display()
    )
}

#[cfg_attr(not(feature = "media"), allow(dead_code))]
fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

/// Puts `output` on the clipboard from a separate Montage process, which
/// outlives the window: on Linux, the program that copied serves the
/// clipboard. Returns once that process is started.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn copy_detached(output: &Path, description: &str) -> Result<()> {
    spawn_self(&[
        "copy-file".as_ref(),
        output.as_os_str(),
        "--description".as_ref(),
        description.as_ref(),
    ])
}
