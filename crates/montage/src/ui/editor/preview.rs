//! The preview: the player's frame, letterboxed, with the captions drawn
//! over it by the same code as the exports, at the preview's size. Captions
//! are dragged into place here.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind,
    TextureHandle, TextureOptions, Vec2,
};
use montage_core::edit::{Caption, Cut};
use montage_media::VideoFrame;
use montage_text::Fonts;

use super::style;
use super::thumbs::to_color_image;

/// How close to the middle a dragged caption snaps to it, as a fraction of
/// the frame's width.
const SNAP: f32 = 0.012;

/// The largest rectangle of aspect `width:height` centred in `avail`.
pub fn fit_rect(avail: Rect, width: u32, height: u32) -> Rect {
    if width == 0 || height == 0 || avail.width() <= 0.0 || avail.height() <= 0.0 {
        return avail;
    }
    let aspect = width as f32 / height as f32;
    let size = if avail.width() / avail.height() > aspect {
        Vec2::new(avail.height() * aspect, avail.height())
    } else {
        Vec2::new(avail.width(), avail.width() / aspect)
    };
    Rect::from_center_size(avail.center(), size)
}

/// Where a caption dragged from `origin` (fractions of the frame) by
/// `delta` points ends up on a `size`-point preview: kept on the frame,
/// snapped to the middle horizontally.
pub fn dragged_position(origin: (f32, f32), delta: Vec2, size: Vec2) -> (f32, f32) {
    if size.x <= 0.0 || size.y <= 0.0 {
        return origin;
    }
    let mut x = (origin.0 + delta.x / size.x).clamp(0.0, 1.0);
    let y = (origin.1 + delta.y / size.y).clamp(0.0, 1.0);
    if (x - 0.5).abs() < SNAP {
        x = 0.5;
    }
    (x, y)
}

/// What changes a caption's look, but not where it is or when.
fn style_key(c: &Caption) -> String {
    format!(
        "{:?}|{}|{}|{}|{}|{}|{}|{}",
        c.text, c.font, c.uppercase, c.size, c.fill, c.outline, c.outline_width, c.shadow
    )
}

struct CachedCaption {
    texture: Option<TextureHandle>,
    /// From the caption's centre to the image's top-left corner, in pixels.
    offset: Vec2,
}

struct CaptionDrag {
    index: usize,
    origin: (f32, f32),
}

#[derive(Default)]
pub struct Preview {
    texture: Option<TextureHandle>,
    captions: HashMap<(String, u32, u32), CachedCaption>,
    drag: Option<CaptionDrag>,
}

pub struct PreviewInput<'a> {
    /// The video's size, for its aspect ratio.
    pub video_size: [u32; 2],
    pub position: f64,
    pub cut: Cut,
    pub fonts: Option<&'a Arc<Mutex<Fonts>>>,
    pub selected: Option<usize>,
    /// Shown instead of the video when there's no frame yet.
    pub message: Option<(String, Color32)>,
}

#[derive(Debug, Default)]
pub struct PreviewOutput {
    /// The video was clicked outside the captions.
    pub toggle_play: bool,
    pub select: Option<usize>,
    /// The video's size on screen, in pixels: what the player should decode
    /// to.
    pub display_px: [u32; 2],
}

impl Preview {
    pub fn upload(&mut self, ctx: &egui::Context, frame: VideoFrame) {
        if frame.image.width == 0 || frame.image.height == 0 {
            return;
        }
        let image = to_color_image(&frame.image);
        match &mut self.texture {
            Some(texture) => texture.set(image, TextureOptions::LINEAR),
            None => {
                self.texture =
                    Some(ctx.load_texture("montage-preview", image, TextureOptions::LINEAR));
            }
        }
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        input: PreviewInput,
        captions: &mut [Caption],
    ) -> PreviewOutput {
        let mut out = PreviewOutput::default();
        let avail = ui.available_rect_before_wrap();
        ui.allocate_rect(avail, Sense::hover());
        let painter = ui.painter_at(avail);
        painter.rect_filled(avail, CornerRadius::same(8), style::PREVIEW_BG);

        let [w, h] = input.video_size;
        let video = fit_rect(avail, w, h);
        let ppp = ui.pixels_per_point();
        out.display_px = [
            (video.width() * ppp).round().max(1.0) as u32,
            (video.height() * ppp).round().max(1.0) as u32,
        ];

        let background = ui.interact(video, ui.scope_id().with("preview-video"), Sense::click());
        if background.clicked() {
            out.toggle_play = true;
        }

        match (&self.texture, &input.message) {
            (Some(texture), _) => {
                let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
                painter.image(texture.id(), video, uv, Color32::WHITE);
            }
            (None, Some((message, color))) => {
                painter.text(
                    video.center(),
                    Align2::CENTER_CENTER,
                    message,
                    FontId::proportional(15.0),
                    *color,
                );
            }
            (None, None) => {
                painter.rect_filled(video, CornerRadius::ZERO, Color32::BLACK);
            }
        }

        if let Some(fonts) = input.fonts {
            self.draw_captions(ui, &painter, video, ppp, fonts, &input, captions, &mut out);
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_captions(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        video: Rect,
        ppp: f32,
        fonts: &Arc<Mutex<Fonts>>,
        input: &PreviewInput,
        captions: &mut [Caption],
        out: &mut PreviewOutput,
    ) {
        let [fw, fh] = out.display_px;
        let mut used = HashSet::new();
        for (i, caption) in captions.iter_mut().enumerate() {
            let selected = input.selected == Some(i);
            let visible = caption.visible_at(input.position, input.cut);
            if caption.text.trim().is_empty() || !(visible || selected) {
                continue;
            }
            let key = (style_key(caption), fw, fh);
            used.insert(key.clone());
            let cached = self.captions.entry(key).or_insert_with(|| {
                // The offset doesn't depend on where the caption is, so it's
                // measured once and reused while it's dragged.
                let rendered = {
                    let mut fonts = fonts.lock().unwrap();
                    montage_text::render_caption(&mut fonts, caption, fw, fh)
                };
                let centre = Vec2::new(caption.x * fw as f32, caption.y * fh as f32);
                let offset = Vec2::new(rendered.x as f32, rendered.y as f32) - centre;
                let texture = (rendered.image.width > 0 && rendered.image.height > 0).then(|| {
                    ui.ctx().load_texture(
                        format!("montage-caption-{i}"),
                        to_color_image(&rendered.image),
                        TextureOptions::LINEAR,
                    )
                });
                CachedCaption { texture, offset }
            });
            let Some(texture) = &cached.texture else {
                continue;
            };
            let [tw, th] = texture.size();
            let top_left_px =
                Vec2::new(caption.x * fw as f32, caption.y * fh as f32) + cached.offset;
            let rect = Rect::from_min_size(
                video.min + top_left_px / ppp,
                Vec2::new(tw as f32, th as f32) / ppp,
            );
            let tint = if visible {
                Color32::WHITE
            } else {
                Color32::from_white_alpha(110)
            };
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            painter.image(texture.id(), rect, uv, tint);

            let response = ui.interact(
                rect,
                ui.scope_id().with(("preview-caption", i)),
                Sense::click_and_drag(),
            );
            if response.hovered() || selected {
                let color = if selected {
                    style::ACCENT
                } else {
                    Color32::from_white_alpha(90)
                };
                painter.rect_stroke(
                    rect.expand(3.0),
                    CornerRadius::same(4),
                    Stroke::new(1.5, color),
                    StrokeKind::Outside,
                );
            }
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
            if response.clicked() || response.drag_started() {
                out.select = Some(i);
            }
            if response.drag_started() {
                self.drag = Some(CaptionDrag {
                    index: i,
                    origin: (caption.x, caption.y),
                });
            }
            if let Some(drag) = self.drag.as_ref().filter(|d| d.index == i)
                && response.dragged()
            {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                let delta = response.total_drag_delta().unwrap_or_default();
                (caption.x, caption.y) = dragged_position(drag.origin, delta, video.size());
                if caption.x == 0.5 {
                    painter.line_segment(
                        [
                            Pos2::new(video.center().x, video.top()),
                            Pos2::new(video.center().x, video.bottom()),
                        ],
                        Stroke::new(1.0, style::ACCENT.gamma_multiply(0.6)),
                    );
                }
            }
            if response.drag_stopped() {
                self.drag = None;
            }
        }
        self.captions.retain(|key, _| used.contains(key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_is_letterboxed() {
        let avail = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(1000.0, 400.0));
        // Wider window: bars left and right.
        let r = fit_rect(avail, 1920, 1080);
        assert!((r.height() - 400.0).abs() < 1e-3);
        assert!((r.width() - 400.0 * 16.0 / 9.0).abs() < 1e-3);
        assert_eq!(r.center(), avail.center());
        // Taller window: bars top and bottom.
        let tall = Rect::from_min_size(Pos2::new(10.0, 10.0), Vec2::new(320.0, 800.0));
        let r = fit_rect(tall, 1920, 1080);
        assert!((r.width() - 320.0).abs() < 1e-3);
        assert!((r.height() - 180.0).abs() < 1e-3);
        assert_eq!(r.center(), tall.center());
        // No size known yet: the whole area.
        assert_eq!(fit_rect(avail, 0, 0), avail);
    }

    #[test]
    fn dragging_moves_by_fractions_of_the_frame() {
        let size = Vec2::new(800.0, 450.0);
        let (x, y) = dragged_position((0.5, 0.12), Vec2::new(200.0, 45.0), size);
        assert!((x - 0.75).abs() < 1e-6);
        assert!((y - 0.22).abs() < 1e-6);
        // Kept on the frame.
        assert_eq!(
            dragged_position((0.9, 0.9), Vec2::new(500.0, 500.0), size),
            (1.0, 1.0)
        );
        assert_eq!(
            dragged_position((0.1, 0.1), Vec2::new(-500.0, -500.0), size),
            (0.0, 0.0)
        );
        // Snaps to the middle, and leaves it again further on.
        assert_eq!(
            dragged_position((0.3, 0.5), Vec2::new(163.0, 0.0), size).0,
            0.5
        );
        let away = dragged_position((0.3, 0.5), Vec2::new(180.0, 0.0), size).0;
        assert!((away - 0.525).abs() < 1e-6);
        assert_eq!(
            dragged_position((0.3, 0.5), Vec2::new(10.0, 0.0), Vec2::ZERO),
            (0.3, 0.5)
        );
    }

    #[test]
    fn position_and_timing_dont_change_the_look() {
        let a = Caption {
            text: "gg".into(),
            ..Caption::default()
        };
        let moved = Caption {
            x: 0.2,
            y: 0.9,
            start: Some(3.0),
            ..a.clone()
        };
        assert_eq!(style_key(&a), style_key(&moved));
        let bigger = Caption {
            size: 0.2,
            ..a.clone()
        };
        assert_ne!(style_key(&a), style_key(&bigger));
    }
}
