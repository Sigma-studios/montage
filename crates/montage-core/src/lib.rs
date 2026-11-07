//! Montage's platform-independent core: clip folders, the edit saved in
//! `montage.toml`, the config, and the Share preset's planning.
//!
//! Nothing here touches libav, the GPU or the desktop, so it builds and is
//! tested the same way on Linux, Windows and macOS.

pub mod clip;
pub mod config;
pub mod edit;
pub mod library;
pub mod presets;
pub mod recorder;
pub mod render;
pub mod share;

pub use clip::{Clip, ExportKind};
pub use config::Config;
pub use edit::Edit;
