// A GUI program on Windows, so opening it from Explorer doesn't flash a
// console; `platform::attach_console` gives the command line its output back.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod controls;
mod platform;
mod share;
#[cfg(feature = "gui")]
mod ui;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use montage_core::{Clip, Config};

use crate::controls::Controls;

/// A clip editor for the replay buffer.
#[derive(Parser)]
#[command(name = "montage", version)]
struct Cli {
    /// Without a command, opens the library.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Opens a clip in the editor.
    Open {
        /// The clip's file, loose or in its folder, or the clip's folder.
        clip: PathBuf,
    },
    /// Exports a clip with the Share preset to the clipboard and notifies.
    Share {
        clip: PathBuf,
        /// Which end a too-long clip keeps. Guessed from the name when
        /// absent: `Replay_…` is a replay. A screenshot isn't shared.
        #[arg(long = "type", value_enum)]
        kind: Option<Kind>,
    },
    /// Exports a clip using its saved edit.
    Export {
        clip: PathBuf,
        #[arg(long, value_enum)]
        preset: Preset,
        /// Where to write the export. Defaults to the clip's folder.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Also puts the export on the clipboard.
        #[arg(long)]
        clipboard: bool,
    },
    /// Starts or stops a recording (Linux, gpu-screen-recorder).
    Record {
        #[arg(value_enum)]
        action: RecordAction,
    },
    /// Saves the replay buffer: `save`, `save 15`, `save 15 edit`,
    /// `save edit 15` (Linux, gpu-screen-recorder). `edit` opens the clip in
    /// the editor instead of sharing it.
    Save {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The recorder's hook, run once a file is written (Linux).
    #[command(name = "on-save")]
    OnSave {
        file: PathBuf,
        /// `replay`, `regular` or `screenshot`.
        kind: String,
    },
    /// The recorder service (Linux).
    Recorder {
        #[command(subcommand)]
        action: RecorderAction,
    },
    /// Opens the settings.
    Settings,
    /// Puts a file on the clipboard and notifies; on Linux, keeps serving the
    /// clipboard until something else is copied. What the editor starts so
    /// the clipboard outlives its window.
    #[command(name = "copy-file", hide = true)]
    CopyFile {
        file: PathBuf,
        /// The notification's text.
        #[arg(long, default_value = "")]
        description: String,
    },
    /// Shows "Clip copied" with an "Edit" action and waits for the click
    /// (Windows and macOS, where `share` itself returns at once).
    #[command(name = "notify-edit", hide = true)]
    NotifyEdit {
        clip: PathBuf,
        #[arg(long, default_value = "")]
        description: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Kind {
    /// Keeps the end.
    Replay,
    /// Keeps the start.
    Regular,
    /// Isn't shared.
    Screenshot,
}

#[derive(Clone, Copy, ValueEnum)]
enum Preset {
    Share,
    Full,
    Gif,
}

#[derive(Clone, Copy, ValueEnum)]
enum RecordAction {
    Start,
    Stop,
    Toggle,
}

#[derive(Subcommand)]
enum RecorderAction {
    /// Runs gpu-screen-recorder with the config's settings, in place of
    /// Montage. What the service starts.
    Run,
}

fn main() -> ExitCode {
    platform::attach_console();
    let mut args: Vec<OsString> = std::env::args_os().collect();
    // Started as the recorder's hook: `montage-on-save <file> <type>`.
    if args
        .first()
        .is_some_and(|a| controls::started_as_on_save(a))
    {
        args.insert(1, "on-save".into());
    }
    let cli = Cli::parse_from(args);
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("montage: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let Some(command) = cli.command else {
        return window(Window::Library);
    };
    match command {
        Command::Open { clip } => {
            Clip::locate(&clip)?;
            window(Window::Editor(clip))
        }
        Command::Share { clip, kind } => {
            if kind == Some(Kind::Screenshot) {
                return Ok(());
            }
            // First, before any thread exists: see `lower_priority`.
            if let Err(e) = platform::lower_priority() {
                eprintln!("montage: can't lower priority, exporting anyway: {e}");
            }
            let kind = kind.map(|k| match k {
                Kind::Replay => montage_core::share::ClipKind::Replay,
                _ => montage_core::share::ClipKind::Regular,
            });
            share::run(&Config::load()?, &clip, kind)
        }
        Command::Export {
            clip,
            preset,
            out,
            clipboard,
        } => {
            let preset = match preset {
                Preset::Share => share::Preset::Share,
                Preset::Full => share::Preset::Full,
                Preset::Gif => share::Preset::Gif,
            };
            let (output, description) =
                share::export(&Config::load()?, &clip, preset, None, out.as_deref())?;
            println!("{}", output.display());
            if clipboard {
                share::copy(&output, &description, None)?;
            }
            Ok(())
        }
        Command::Settings => window(Window::Settings),
        Command::CopyFile { file, description } => share::copy(&file, &description, None),
        Command::NotifyEdit { clip, description } => share::wait_for_edit(&clip, &description),
        Command::Record { action } => {
            let controls = recorder_controls()?;
            match action {
                RecordAction::Start => controls.record_start(),
                RecordAction::Stop => controls.record_stop(),
                RecordAction::Toggle => controls.record_toggle(),
            }
        }
        Command::Save { args } => recorder_controls()?.save(&args),
        Command::OnSave { file, kind } => recorder_controls()?.on_save(&file, &kind),
        Command::Recorder {
            action: RecorderAction::Run,
        } => recorder_controls()?.recorder_run(),
    }
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
enum Window {
    Library,
    Editor(PathBuf),
    Settings,
}

#[cfg(feature = "gui")]
fn window(window: Window) -> Result<()> {
    ui::run(match window {
        Window::Library => ui::Start::Library,
        Window::Editor(path) => ui::Start::Editor(path),
        Window::Settings => ui::Start::Settings,
    })
}

#[cfg(not(feature = "gui"))]
fn window(_window: Window) -> Result<()> {
    bail!("this build has no window")
}

fn recorder_controls() -> Result<Controls> {
    if !cfg!(target_os = "linux") {
        bail!("the recorder controls drive gpu-screen-recorder, which only runs on Linux");
    }
    Controls::from_env(Config::load()?, Box::new(platform::notify))
}
