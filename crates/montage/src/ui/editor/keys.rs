//! Keyboard shortcuts: which key does what, and the J/K/L shuttle.

use eframe::egui::{Key, Modifiers};

/// What a key press asks the editor to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    PlayPause,
    /// Steps this many frames, paused.
    Step(i32),
    ShuttleBack,
    ShuttlePause,
    ShuttleForward,
    SetCutStart,
    SetCutEnd,
    /// Back to the library, after saving.
    Back,
    /// Saves the edit now (Cmd+S on macOS, Ctrl+S elsewhere).
    Save,
    /// Starts the export with the current settings (Cmd+E / Ctrl+E).
    Export,
}

/// The action for a key press, if any. `repeat` is true for the presses a
/// held key sends: only frame steps repeat.
pub fn action_for(key: Key, modifiers: Modifiers, repeat: bool) -> Option<Action> {
    if modifiers.command && !modifiers.alt && !modifiers.shift {
        return match (key, repeat) {
            (Key::S, false) => Some(Action::Save),
            (Key::E, false) => Some(Action::Export),
            _ => None,
        };
    }
    // Ctrl on macOS, where it isn't the command key, and Alt never take the
    // plain shortcuts.
    if modifiers.command || modifiers.ctrl || modifiers.alt || modifiers.mac_cmd {
        return None;
    }
    // Shift is allowed for the steps: on AZERTY, `.` is Shift+;.
    let action = match key {
        Key::Comma => Action::Step(-1),
        Key::Period => Action::Step(1),
        _ if modifiers.shift => return None,
        Key::Space => Action::PlayPause,
        Key::J => Action::ShuttleBack,
        Key::K => Action::ShuttlePause,
        Key::L => Action::ShuttleForward,
        Key::I => Action::SetCutStart,
        Key::O => Action::SetCutEnd,
        Key::Escape => Action::Back,
        _ => return None,
    };
    if repeat && !matches!(action, Action::Step(_)) {
        return None;
    }
    Some(action)
}

/// The fastest shuttle speed, either way.
pub const MAX_SHUTTLE: f64 = 8.0;

/// J/K/L: L plays forwards, faster with each press; J the same backwards;
/// K pauses.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Shuttle {
    /// 0 when paused, negative backwards.
    pub speed: f64,
}

impl Shuttle {
    /// Applies a shuttle action and returns the new speed (0 means pause).
    pub fn press(&mut self, action: Action) -> f64 {
        self.speed = match action {
            Action::ShuttleForward if self.speed > 0.0 => (self.speed * 2.0).min(MAX_SHUTTLE),
            Action::ShuttleForward => 1.0,
            Action::ShuttleBack if self.speed < 0.0 => (self.speed * 2.0).max(-MAX_SHUTTLE),
            Action::ShuttleBack => -1.0,
            Action::ShuttlePause => 0.0,
            _ => self.speed,
        };
        self.speed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;

    #[test]
    fn plain_keys_map_to_actions() {
        assert_eq!(action_for(Key::Space, NONE, false), Some(Action::PlayPause));
        assert_eq!(action_for(Key::Comma, NONE, false), Some(Action::Step(-1)));
        assert_eq!(action_for(Key::Period, NONE, false), Some(Action::Step(1)));
        assert_eq!(action_for(Key::J, NONE, false), Some(Action::ShuttleBack));
        assert_eq!(action_for(Key::K, NONE, false), Some(Action::ShuttlePause));
        assert_eq!(
            action_for(Key::L, NONE, false),
            Some(Action::ShuttleForward)
        );
        assert_eq!(action_for(Key::I, NONE, false), Some(Action::SetCutStart));
        assert_eq!(action_for(Key::O, NONE, false), Some(Action::SetCutEnd));
        assert_eq!(action_for(Key::Escape, NONE, false), Some(Action::Back));
        assert_eq!(action_for(Key::A, NONE, false), None);
    }

    #[test]
    fn only_steps_repeat() {
        assert_eq!(action_for(Key::Period, NONE, true), Some(Action::Step(1)));
        assert_eq!(action_for(Key::Space, NONE, true), None);
        assert_eq!(action_for(Key::L, NONE, true), None);
    }

    #[test]
    fn modifiers_use_the_command_key() {
        assert_eq!(
            action_for(Key::S, Modifiers::COMMAND, false),
            Some(Action::Save)
        );
        assert_eq!(
            action_for(Key::E, Modifiers::COMMAND, false),
            Some(Action::Export)
        );
        assert_eq!(action_for(Key::S, NONE, false), None);
        // Command+I isn't I.
        assert_eq!(action_for(Key::I, Modifiers::COMMAND, false), None);
        assert_eq!(action_for(Key::Space, Modifiers::ALT, false), None);
        // Shift+. (AZERTY) still steps, Shift+Space does nothing.
        assert_eq!(
            action_for(Key::Period, Modifiers::SHIFT, false),
            Some(Action::Step(1))
        );
        assert_eq!(action_for(Key::Space, Modifiers::SHIFT, false), None);
    }

    #[test]
    fn shuttle_speeds_up_and_reverses() {
        let mut s = Shuttle::default();
        assert_eq!(s.press(Action::ShuttleForward), 1.0);
        assert_eq!(s.press(Action::ShuttleForward), 2.0);
        assert_eq!(s.press(Action::ShuttleForward), 4.0);
        assert_eq!(s.press(Action::ShuttleForward), 8.0);
        assert_eq!(s.press(Action::ShuttleForward), 8.0);
        assert_eq!(s.press(Action::ShuttleBack), -1.0);
        assert_eq!(s.press(Action::ShuttleBack), -2.0);
        assert_eq!(s.press(Action::ShuttlePause), 0.0);
        assert_eq!(s.press(Action::ShuttleBack), -1.0);
        assert_eq!(s.press(Action::ShuttleForward), 1.0);
    }
}
