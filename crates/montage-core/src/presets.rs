//! Settings of the export presets other than Share (see `share`).

use serde::{Deserialize, Serialize};

/// The `[gif]` section of `config.toml`: the GIF preset's starting point.
/// The export lowers the width, then the frame rate, until it fits.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GifSettings {
    pub width: u32,
    pub fps: u32,
    /// The size target, in bytes.
    pub max_bytes: u64,
}

impl Default for GifSettings {
    fn default() -> Self {
        GifSettings {
            width: 480,
            fps: 20,
            max_bytes: 10 * 1000 * 1000,
        }
    }
}
