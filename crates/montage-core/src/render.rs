//! What the preview and the exports share for drawing over the video, so
//! they match pixel for pixel.

/// A straight (not premultiplied) RGBA8 image, rows packed, top row first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl RgbaImage {
    /// A fully transparent image.
    pub fn new(width: u32, height: u32) -> RgbaImage {
        RgbaImage {
            width,
            height,
            data: vec![0; width as usize * height as usize * 4],
        }
    }
}

/// Something drawn over the video: the captions. The exports call it on
/// every frame at the export's size; the preview calls it at the preview's.
pub trait Overlay: Send {
    /// Draws what shows at `t` (seconds of the original) onto `frame`.
    fn draw(&mut self, t: f64, frame: &mut RgbaImage);

    /// True when nothing ever shows, so exports can skip compositing (and
    /// Full quality can copy the stream).
    fn is_empty(&self) -> bool;
}
