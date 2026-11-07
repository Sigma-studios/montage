//! Putting a file on the clipboard, so pasting it into Discord or a browser
//! uploads it. No cross-platform crate does files, so each system has its
//! own few lines.
//!
//! [`copy_file`] runs `copied` once the clipboard holds the file. On Linux,
//! Wayland's clipboard is served by the program that copied, so `copy_file`
//! then keeps serving it until something else is copied; `montage share`
//! runs in a systemd unit of its own for that reason. Windows and macOS keep
//! the data themselves, and `copy_file` returns at once.

use std::path::Path;

use anyhow::Result;

pub fn copy_file(path: &Path, copied: impl FnOnce()) -> Result<()> {
    imp::copy_file(path, copied)
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::Path;

    use anyhow::{Context, Result};
    use wl_clipboard_rs::copy::{MimeType, Options, ServeRequests, Source};

    pub fn copy_file(path: &Path, copied: impl FnOnce()) -> Result<()> {
        let uri_list = format!("{}\r\n", super::file_uri(path));
        let mut options = Options::new();
        options
            .serve_requests(ServeRequests::Unlimited)
            .foreground(true);
        let prepared = options
            .prepare_copy(
                Source::Bytes(uri_list.into_bytes().into_boxed_slice()),
                MimeType::Specific("text/uri-list".into()),
            )
            .context("can't take the Wayland clipboard")?;
        copied();
        prepared
            .serve()
            .context("can't serve the Wayland clipboard")
    }
}

#[cfg(windows)]
mod imp {
    use std::path::Path;

    use anyhow::{Context, Result, anyhow};
    use clipboard_win::{Clipboard, raw};

    /// `CF_HDROP`, what Explorer's Copy puts on the clipboard.
    pub fn copy_file(path: &Path, copied: impl FnOnce()) -> Result<()> {
        let path = path
            .to_str()
            .with_context(|| format!("{} isn't valid Unicode", path.display()))?;
        let _open =
            Clipboard::new_attempts(10).map_err(|e| anyhow!("can't open the clipboard: {e}"))?;
        raw::empty().map_err(|e| anyhow!("can't empty the clipboard: {e}"))?;
        raw::set_file_list(&[path]).map_err(|e| anyhow!("can't copy the file: {e}"))?;
        copied();
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::Path;

    use anyhow::{Context, Result, bail};
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSString, NSURL};

    /// A file URL on the general pasteboard, as Finder's Copy does.
    pub fn copy_file(path: &Path, copied: impl FnOnce()) -> Result<()> {
        let path = path
            .to_str()
            .with_context(|| format!("{} isn't valid Unicode", path.display()))?;
        let url = NSURL::fileURLWithPath(&NSString::from_str(path));
        let object: objc2::rc::Retained<ProtocolObject<dyn NSPasteboardWriting>> =
            ProtocolObject::from_retained(url);
        let objects = NSArray::from_retained_slice(&[object]);
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        if !pasteboard.writeObjects(&objects) {
            bail!("the pasteboard refused the file");
        }
        copied();
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod imp {
    use std::path::Path;

    pub fn copy_file(_path: &Path, _copied: impl FnOnce()) -> anyhow::Result<()> {
        anyhow::bail!("the clipboard isn't supported on this system")
    }
}

/// `file://` and the path, percent-encoded but for unreserved characters
/// and slashes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn file_uri(path: &Path) -> String {
    let bytes = path.as_os_str().as_encoded_bytes();
    let mut uri = String::from("file://");
    for &b in bytes {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn file_uris_are_percent_encoded() {
        assert_eq!(
            file_uri(Path::new("/home/a/Videos/Replay_x-share.mp4")),
            "file:///home/a/Videos/Replay_x-share.mp4"
        );
        assert_eq!(
            file_uri(Path::new("/home/a/Mes vidéos/50%.mp4")),
            "file:///home/a/Mes%20vid%C3%A9os/50%25.mp4"
        );
    }
}
