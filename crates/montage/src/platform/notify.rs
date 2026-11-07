//! The notification after a share, with its "Edit" action, which opens the
//! clip in the editor.
//!
//! [`notify_with_edit`] blocks until the notification is acted on, closed,
//! or (Windows, macOS) [`WAIT`] has passed, so callers run it on a thread
//! of a process that stays alive (the Linux share, which serves the
//! clipboard) or in a detached copy of Montage (Windows, macOS).

use std::time::Duration;

/// How long Windows and macOS wait for a click.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub const WAIT: Duration = Duration::from_secs(60);

/// Shows the notification; runs `on_edit` if its "Edit" action (or, where
/// buttons aren't available, the notification itself) is clicked.
pub fn notify_with_edit(summary: &str, body: &str, on_edit: impl FnOnce() + Send + 'static) {
    if let Err(e) = imp::notify_with_edit(summary, body, on_edit) {
        eprintln!("montage: can't notify “{summary}”: {e}");
    }
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn notify_with_edit(
        summary: &str,
        body: &str,
        on_edit: impl FnOnce() + Send + 'static,
    ) -> Result<(), String> {
        let handle = notify_rust::Notification::new()
            .appname(super::super::APP_NAME)
            .summary(summary)
            .body(body)
            .action("default", "Edit")
            .action("edit", "Edit")
            .show()
            .map_err(|e| e.to_string())?;
        handle.wait_for_action(|action| {
            if action == "edit" || action == "default" {
                on_edit();
            }
        });
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::mpsc;

    use tauri_winrt_notification::Toast;

    /// A toast under PowerShell's app ID, which needs no Start menu
    /// shortcut; its button and body both open the editor.
    pub fn notify_with_edit(
        summary: &str,
        body: &str,
        on_edit: impl FnOnce() + Send + 'static,
    ) -> Result<(), String> {
        let (clicked, click) = mpsc::channel();
        Toast::new(Toast::POWERSHELL_APP_ID)
            .title(summary)
            .text1(body)
            .add_button("Edit", "edit")
            .on_activated(move |_action| {
                let _ = clicked.send(());
                Ok(())
            })
            .show()
            .map_err(|e| e.to_string())?;
        if click.recv_timeout(super::WAIT).is_ok() {
            on_edit();
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use mac_notification_sys::{MainButton, Notification, NotificationResponse};

    /// Buttons need a signed app bundle; until Montage is signed, macOS may
    /// show none, and clicking the notification opens the editor.
    pub fn notify_with_edit(
        summary: &str,
        body: &str,
        on_edit: impl FnOnce() + Send + 'static,
    ) -> Result<(), String> {
        // `wait_for_click` blocks until a click or a dismissal, which may
        // never come: wait on a thread, at most WAIT.
        let (summary, body) = (summary.to_string(), body.to_string());
        let (done, response) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let sent = Notification::new()
                .title(&summary)
                .message(&body)
                .main_button(MainButton::SingleAction("Edit"))
                .wait_for_click(true)
                .send()
                .map_err(|e| e.to_string());
            let _ = done.send(sent);
        });
        match response.recv_timeout(super::WAIT) {
            Ok(Ok(NotificationResponse::ActionButton(_) | NotificationResponse::Click)) => {
                on_edit();
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod imp {
    pub fn notify_with_edit(
        summary: &str,
        body: &str,
        _on_edit: impl FnOnce() + Send + 'static,
    ) -> Result<(), String> {
        eprintln!("{summary}: {body}");
        Ok(())
    }
}
