//! What the settings page can offer: the screens and audio sources
//! gpu-screen-recorder lists, and the screens' names from Wayland.
//!
//! The lists' formats, from gpu-screen-recorder's `main.cpp`:
//!
//! - `--list-capture-options`: one option per line. `window` and `focused`
//!   (X11 only), one `NAME|WIDTHxHEIGHT` line per monitor (`DP-1|2560x1440`),
//!   `region` when there are monitors, one `PATH|WxH@Fhz|FORMAT` line per
//!   camera mode (`/dev/video0|1280x720@30hz|mjpeg`), and `portal`.
//! - `--list-audio-devices`: `default_output|Default output`,
//!   `default_input|Default input`, then one `NAME|DESCRIPTION` line per
//!   PulseAudio/PipeWire source.
//! - `--list-application-audio`: one application name per line.
//!
//! The parsers take what they don't recognise as a plain name, so a change
//! of format degrades to less helpful labels, not to missing entries.

use std::ffi::OsStr;
use std::io;
use std::process::{Command, Stdio};

/// A monitor gpu-screen-recorder can capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    /// The connector name: `DP-5`.
    pub name: String,
    /// `2560x1440`, when listed.
    pub size: Option<String>,
}

/// An audio device: its name in `-a`, and what to call it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    pub name: String,
    pub description: String,
}

/// A Wayland output: its connector name and the monitor's model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub name: String,
    pub model: String,
}

/// Everything the page looked up when it opened. Each list is an error
/// message when it couldn't be had.
#[derive(Debug, Clone)]
pub struct Probe {
    pub monitors: Result<Vec<Monitor>, String>,
    pub audio_devices: Result<Vec<AudioDevice>, String>,
    pub apps: Result<Vec<String>, String>,
    pub outputs: Vec<Output>,
}

/// Capture options that aren't monitors.
const NOT_MONITORS: &[&str] = &["window", "focused", "region", "portal", "screen"];

pub fn parse_capture_options(text: &str) -> Vec<Monitor> {
    let mut monitors: Vec<Monitor> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut fields = line.split('|');
        let name = fields.next().unwrap_or_default().trim();
        if name.is_empty() || NOT_MONITORS.contains(&name) || name.starts_with('/') {
            // Keywords and cameras (`/dev/video0|…`).
            continue;
        }
        if name.starts_with("gsr ") || name.contains(' ') {
            // A message, not an option.
            continue;
        }
        let size = fields
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        if !monitors.iter().any(|m| m.name == name) {
            monitors.push(Monitor {
                name: name.to_string(),
                size,
            });
        }
    }
    monitors
}

pub fn parse_audio_devices(text: &str) -> Vec<AudioDevice> {
    let mut devices: Vec<AudioDevice> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (name, description) = match line.split_once('|') {
            Some((name, description)) => (name.trim(), description.trim()),
            None => (line, line),
        };
        if name.is_empty() || devices.iter().any(|d| d.name == name) {
            continue;
        }
        devices.push(AudioDevice {
            name: name.to_string(),
            description: if description.is_empty() {
                name.to_string()
            } else {
                description.to_string()
            },
        });
    }
    devices
}

pub fn parse_apps(text: &str) -> Vec<String> {
    let mut apps: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !apps.iter().any(|a| a == line) {
            apps.push(line.to_string());
        }
    }
    apps
}

/// The model in an output's description. Compositors write
/// `Dell Inc. DELL U2720Q 8H2M123 (DP-5)`: the connector in brackets is
/// dropped, the rest kept.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn output_model(name: &str, description: &str, make: &str, model: &str) -> String {
    let description = description.trim();
    let description = description
        .strip_suffix(&format!("({name})"))
        .unwrap_or(description)
        .trim();
    if !description.is_empty() && description != name {
        return description.to_string();
    }
    let make_model = format!("{} {}", make.trim(), model.trim());
    let make_model = make_model.trim();
    if make_model.is_empty() || make_model.eq_ignore_ascii_case("unknown unknown") {
        String::new()
    } else {
        make_model.to_string()
    }
}

/// Runs `recorder flag` and returns its output.
fn list(recorder: &OsStr, flag: &str) -> Result<String, String> {
    let output = Command::new(recorder)
        .arg(flag)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => "gpu-screen-recorder isn't installed".to_string(),
            _ => format!("can't run gpu-screen-recorder: {e}"),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(format!(
            "gpu-screen-recorder {flag} failed ({}){}{first}",
            output.status,
            if first.is_empty() { "" } else { ": " }
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Looks everything up. Slow-ish (gpu-screen-recorder opens the GPU and
/// the audio server), so run on a thread.
pub fn run(recorder: &OsStr) -> Probe {
    Probe {
        monitors: list(recorder, "--list-capture-options").map(|t| parse_capture_options(&t)),
        audio_devices: list(recorder, "--list-audio-devices").map(|t| parse_audio_devices(&t)),
        apps: list(recorder, "--list-application-audio").map(|t| parse_apps(&t)),
        outputs: outputs(),
    }
}

#[cfg(target_os = "linux")]
fn outputs() -> Vec<Output> {
    super::wayland::outputs().unwrap_or_default()
}

#[cfg(not(target_os = "linux"))]
fn outputs() -> Vec<Output> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_options_keep_the_monitors() {
        let text = "window\nfocused\nDP-5|2560x1440\nHDMI-A-1|1920x1080\nregion\n\
                    /dev/video0|1280x720@30hz|mjpeg\n/dev/video0|640x480@30hz|yuyv\nportal\n";
        assert_eq!(
            parse_capture_options(text),
            [
                Monitor {
                    name: "DP-5".into(),
                    size: Some("2560x1440".into())
                },
                Monitor {
                    name: "HDMI-A-1".into(),
                    size: Some("1920x1080".into())
                },
            ]
        );
    }

    #[test]
    fn capture_options_tolerate_plain_names_and_noise() {
        let text = "  eDP-1  \n\nDP-2|\nDP-2|3840x2160\ngsr warning: something\n";
        assert_eq!(
            parse_capture_options(text),
            [
                Monitor {
                    name: "eDP-1".into(),
                    size: None
                },
                Monitor {
                    name: "DP-2".into(),
                    size: None
                },
            ]
        );
        assert!(parse_capture_options("").is_empty());
    }

    #[test]
    fn audio_devices_have_names_and_descriptions() {
        let text = "default_output|Default output\ndefault_input|Default input\n\
                    alsa_output.pci-0000_0c_00.4.analog-stereo.monitor|Monitor of Starship/Matisse HD Audio\n\
                    alsa_input.usb-Blue_Yeti|Yeti Stereo Microphone | Analog\n\
                    bare_name\n";
        let devices = parse_audio_devices(text);
        assert_eq!(devices.len(), 5);
        assert_eq!(devices[0].name, "default_output");
        assert_eq!(devices[0].description, "Default output");
        assert_eq!(
            devices[2].description,
            "Monitor of Starship/Matisse HD Audio"
        );
        // Only the first `|` separates: descriptions may hold one.
        assert_eq!(devices[3].name, "alsa_input.usb-Blue_Yeti");
        assert_eq!(devices[3].description, "Yeti Stereo Microphone | Analog");
        assert_eq!(devices[4].name, "bare_name");
        assert_eq!(devices[4].description, "bare_name");
    }

    #[test]
    fn apps_are_one_per_line() {
        assert_eq!(
            parse_apps("firefox\n\nDiscord\nfirefox\n  steam \n"),
            ["firefox", "Discord", "steam"]
        );
    }

    #[test]
    fn output_models_drop_the_connector() {
        assert_eq!(
            output_model("DP-5", "Dell Inc. DELL U2720Q 8H2M123 (DP-5)", "", ""),
            "Dell Inc. DELL U2720Q 8H2M123"
        );
        assert_eq!(
            output_model("DP-5", "", "Dell Inc.", "DELL U2720Q"),
            "Dell Inc. DELL U2720Q"
        );
        assert_eq!(output_model("DP-5", "DP-5", "Unknown", "Unknown"), "");
    }

    #[test]
    fn a_missing_recorder_is_reported() {
        let err = list(
            OsStr::new("/nonexistent/gpu-screen-recorder"),
            "--list-audio-devices",
        )
        .unwrap_err();
        assert_eq!(err, "gpu-screen-recorder isn't installed");
    }
}
