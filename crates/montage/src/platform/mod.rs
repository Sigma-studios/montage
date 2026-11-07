//! What differs between Linux, Windows and macOS, one file per system behind
//! the same small functions. Everything else in Montage is shared.

use std::io;

pub mod clipboard;
pub mod notify;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as imp;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod imp {
    pub fn lower_priority() -> std::io::Result<()> {
        Ok(())
    }
}

/// Lowers the process's CPU and disk priority, so a headless export doesn't
/// cost the game, which is usually still running, any frames.
///
/// Call it before starting any thread: on Linux the priority is per thread
/// and only threads created afterwards inherit it.
pub fn lower_priority() -> io::Result<()> {
    imp::lower_priority()
}

/// The app name notifications carry.
pub const APP_NAME: &str = "Montage";

/// Shows a desktop notification. On the systems where that isn't wired up
/// yet, prints it.
pub fn notify(summary: &str, body: &str) {
    #[cfg(target_os = "linux")]
    {
        let shown = notify_rust::Notification::new()
            .appname(APP_NAME)
            .summary(summary)
            .body(body)
            .show();
        if let Err(e) = shown {
            eprintln!("montage: can't notify “{summary}”: {e}");
        }
    }
    #[cfg(not(target_os = "linux"))]
    eprintln!("{APP_NAME}: {summary}: {body}");
}

/// On Windows, Montage is a GUI program so Explorer doesn't flash a console;
/// this attaches it to the terminal it was started from, if any, so the
/// command line still prints. Does nothing elsewhere.
pub fn attach_console() {
    #[cfg(windows)]
    imp::attach_parent_console();
}
