//! What the editor needs from the player, as a trait so the view can be
//! drawn in tests without decoding anything.

use montage_media::{Player, VideoFrame};

pub trait Playback {
    fn decoder_name(&self) -> String;
    /// False when the system has no audio output (or the clip no audio).
    fn has_audio_output(&self) -> bool;
    fn play(&mut self);
    fn pause(&mut self);
    fn is_playing(&self) -> bool;
    fn set_speed(&mut self, speed: f64);
    fn seek(&mut self, t: f64);
    fn step(&mut self, frames: i32);
    fn position(&self) -> f64;
    fn set_gains(&mut self, gains: &[f32]);
    fn set_loop(&mut self, range: Option<(f64, f64)>);
    fn set_preview_size(&mut self, max_width: u32, max_height: u32);
    fn take_frame(&mut self) -> Option<VideoFrame>;
}

impl Playback for Player {
    fn decoder_name(&self) -> String {
        Player::decoder_name(self)
    }
    fn has_audio_output(&self) -> bool {
        Player::has_audio_output(self)
    }
    fn play(&mut self) {
        Player::play(self)
    }
    fn pause(&mut self) {
        Player::pause(self)
    }
    fn is_playing(&self) -> bool {
        Player::is_playing(self)
    }
    fn set_speed(&mut self, speed: f64) {
        Player::set_speed(self, speed)
    }
    fn seek(&mut self, t: f64) {
        Player::seek(self, t)
    }
    fn step(&mut self, frames: i32) {
        Player::step(self, frames)
    }
    fn position(&self) -> f64 {
        Player::position(self)
    }
    fn set_gains(&mut self, gains: &[f32]) {
        Player::set_gains(self, gains)
    }
    fn set_loop(&mut self, range: Option<(f64, f64)>) {
        Player::set_loop(self, range)
    }
    fn set_preview_size(&mut self, max_width: u32, max_height: u32) {
        Player::set_preview_size(self, max_width, max_height)
    }
    fn take_frame(&mut self) -> Option<VideoFrame> {
        Player::take_frame(self)
    }
}

/// The player's state in the view: opened on the first frame, once the
/// preview's size is known.
pub enum PlayerSlot {
    Pending,
    Open(Box<dyn Playback>),
    Failed(String),
}

impl PlayerSlot {
    pub fn get(&mut self) -> Option<&mut (dyn Playback + 'static)> {
        match self {
            PlayerSlot::Open(player) => Some(player.as_mut()),
            _ => None,
        }
    }

    pub fn get_ref(&self) -> Option<&(dyn Playback + 'static)> {
        match self {
            PlayerSlot::Open(player) => Some(player.as_ref()),
            _ => None,
        }
    }
}

/// A player that shows one still frame, for tests.
#[cfg(test)]
pub struct FakePlayer {
    pub position: f64,
    pub playing: bool,
    pub frame: Option<VideoFrame>,
    pub gains: Vec<f32>,
    pub looped: Option<(f64, f64)>,
    pub speed: f64,
}

#[cfg(test)]
impl FakePlayer {
    pub fn new(frame: montage_core::render::RgbaImage) -> FakePlayer {
        FakePlayer {
            position: 0.0,
            playing: false,
            frame: Some(VideoFrame {
                pts: 0.0,
                image: frame,
            }),
            gains: Vec::new(),
            looped: None,
            speed: 1.0,
        }
    }
}

#[cfg(test)]
impl Playback for FakePlayer {
    fn decoder_name(&self) -> String {
        "hevc (test)".into()
    }
    fn has_audio_output(&self) -> bool {
        true
    }
    fn play(&mut self) {
        self.playing = true;
    }
    fn pause(&mut self) {
        self.playing = false;
    }
    fn is_playing(&self) -> bool {
        self.playing
    }
    fn set_speed(&mut self, speed: f64) {
        self.speed = speed;
    }
    fn seek(&mut self, t: f64) {
        self.position = t;
    }
    fn step(&mut self, frames: i32) {
        self.playing = false;
        self.position += f64::from(frames) / 60.0;
    }
    fn position(&self) -> f64 {
        self.position
    }
    fn set_gains(&mut self, gains: &[f32]) {
        self.gains = gains.to_vec();
    }
    fn set_loop(&mut self, range: Option<(f64, f64)>) {
        self.looped = range;
    }
    fn set_preview_size(&mut self, _max_width: u32, _max_height: u32) {}
    fn take_frame(&mut self) -> Option<VideoFrame> {
        self.frame.take()
    }
}
