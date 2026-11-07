//! Captions: every installed font (through fontdb, which knows the font
//! folders of Linux, Windows and macOS), and text drawn with cosmic-text's
//! layout and tiny-skia's fill and stroke, which gives the thick outline of
//! the classic Impact caption. The preview and the exports use the same code,
//! so they match pixel for pixel.
//!
//! The glyphs are never rasterised by the font engine: cosmic-text shapes and
//! wraps the text, swash (through cosmic-text's [`SwashCache`]) gives each
//! glyph's outline as path commands, and tiny-skia fills and strokes the
//! whole caption as one path. The outline is therefore a real stroke with
//! round joins, drawn under the fill.

mod draw;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use cosmic_text::skrifa::outline::{DrawSettings, OutlinePen};
use cosmic_text::skrifa::prelude::{FontRef, GlyphId, LocationRef, MetadataProvider, Size};
use cosmic_text::{
    Align, Attrs, Buffer, CacheKey, CacheKeyFlags, Command, Family, FontSystem, Metrics, Shaping,
    Stretch, Style, SubpixelBin, SwashCache, Weight, Wrap,
};
use montage_core::edit::{Caption, Color, Cut, DEFAULT_FONT};
use montage_core::render::{Overlay, RgbaImage};
use tiny_skia::{Path, PathBuilder, Rect, Transform};

/// Families tried, in order, when neither the caption's font nor Impact is
/// installed. Between them they cover the usual Linux, Windows and macOS
/// installs.
const SANS_SERIF: &[&str] = &[
    "DejaVu Sans",
    "Liberation Sans",
    "Arial",
    "Helvetica",
    "Noto Sans",
    "Segoe UI",
    "Open Sans",
    "Roboto",
    "Ubuntu",
    "Cantarell",
    "Verdana",
    "Helvetica Neue",
    "Tahoma",
];

/// Share of the frame's width a caption may take before it wraps.
const WRAP_WIDTH: f32 = 0.9;
/// Shadow offset (right and down), as a fraction of the caption's size.
const SHADOW_OFFSET: f32 = 0.04;
/// Shadow blur radius, as a fraction of the caption's size.
const SHADOW_BLUR: f32 = 0.015;
/// Shadow opacity, out of 255 (about 60 %).
const SHADOW_ALPHA: u8 = 153;
/// Larger renders are refused (an empty caption is returned instead).
const MAX_PIXELS: u64 = 64 << 20;

/// The installed fonts. Loading them all takes a moment (about 2,900
/// families on the Linux desktop), so the editor does it on a thread.
pub struct Fonts {
    system: FontSystem,
    scaler: SwashCache,
    /// Shown in the font picker.
    families: Vec<String>,
    /// Every name of every face (localised ones included), lowercased, to
    /// the name as the font spells it, which is what fontdb matches.
    names: HashMap<String, String>,
    /// The family used when neither the asked one nor Impact is installed.
    sans_serif: Option<String>,
}

impl Fonts {
    pub fn load() -> Fonts {
        let mut system = FontSystem::new();
        let mut names = HashMap::new();
        let mut families = Vec::new();
        for face in system.db().faces() {
            for (name, _) in &face.families {
                names
                    .entry(name.to_lowercase())
                    .or_insert_with(|| name.clone());
            }
            // The first name is the English one when the font has it. Names
            // starting with a dot are macOS's hidden system fonts.
            if let Some((name, _)) = face.families.first()
                && !name.trim().is_empty()
                && !name.starts_with('.')
            {
                families.push(name.clone());
            }
        }
        families.sort_by_cached_key(|name| (name.to_lowercase(), name.clone()));
        families.dedup_by(|a, b| a.to_lowercase() == b.to_lowercase());

        let sans_serif = SANS_SERIF
            .iter()
            .find_map(|name| names.get(&name.to_lowercase()).cloned())
            .or_else(|| families.first().cloned());
        if let Some(sans) = &sans_serif {
            // cosmic-text's default ("Open Sans") is often missing.
            system.db_mut().set_sans_serif_family(sans.clone());
        }

        Fonts {
            system,
            scaler: SwashCache::new(),
            families,
            names,
            sans_serif,
        }
    }

    /// Every family name, sorted case-insensitively, without duplicates.
    pub fn families(&self) -> &[String] {
        &self.families
    }

    /// Whether `family` is installed, ignoring case. Localised names count.
    pub fn has_family(&self, family: &str) -> bool {
        self.names.contains_key(&family.to_lowercase())
    }

    /// The family drawn for `family`, and whether it's a fallback: the
    /// installed spelling of `family` when it's installed (whatever the case
    /// asked), else Impact, else a sans-serif (DejaVu Sans, Liberation Sans,
    /// Arial, Helvetica… or failing all those the first installed family).
    pub fn resolve_family(&self, family: &str) -> (&str, bool) {
        if let Some(name) = self.names.get(&family.to_lowercase()) {
            return (name, false);
        }
        let fallback = self
            .names
            .get(&DEFAULT_FONT.to_lowercase())
            .or(self.sans_serif.as_ref())
            .map_or("sans-serif", String::as_str);
        (fallback, true)
    }

    /// `text` drawn in `family` at `px` pixels, for the font picker, where
    /// each name is shown in its own font. Falls back to the default font
    /// when the family is missing, or lacks some of the characters.
    ///
    /// The image is the text's tight bounding box (so baselines differ from
    /// one label to the next); empty text gives a 1x1 transparent image.
    ///
    /// Fast enough to call for each visible row (a fraction of a
    /// millisecond for a short name): the glyphs are laid side by side from
    /// the font's character map and advances, skipping cosmic-text's shaping
    /// and font matching, which scan every installed face for each new
    /// family. No ligatures or kerning, which a font name doesn't need.
    pub fn render_label(&mut self, family: &str, text: &str, px: f32, color: Color) -> RgbaImage {
        let family = self.resolve_family(family).0.to_owned();
        let text = text.replace(['\r', '\n'], " ");
        if !px.is_finite() || px <= 0.0 || text.trim().is_empty() {
            return RgbaImage::new(1, 1);
        }
        let path = match self.quick_line(&family, &text, px) {
            Some(path) => path,
            None => {
                let default = self.resolve_family(DEFAULT_FONT).0.to_owned();
                match self.quick_line(&default, &text, px) {
                    Some(path) => path,
                    // Neither has every character: shape it properly, with
                    // cosmic-text's per-character fallbacks.
                    None => self.shape(&family, &text, px, px * 1.25, None).path,
                }
            }
        };
        let rect = path
            .as_ref()
            .and_then(Path::compute_tight_bounds)
            .and_then(|b| PixelRect::around(b, 0.0));
        match (path, rect) {
            (Some(path), Some(rect)) => draw::rasterize(&path, rect, color, None, None),
            _ => RgbaImage::new(1, 1),
        }
    }

    /// `text` on one line in `family`'s regular face, baseline at y = 0,
    /// starting at x = 0, or `None` when the face is missing or lacks a
    /// character.
    fn quick_line(&self, family: &str, text: &str, px: f32) -> Option<Option<Path>> {
        let db = self.system.db();
        let id = self.face_id(family)?;
        db.with_face_data(id, |data, index| {
            let font = FontRef::from_index(data, index).ok()?;
            let charmap = font.charmap();
            let size = Size::new(px);
            let advances = font.glyph_metrics(size, LocationRef::default());
            let outlines = font.outline_glyphs();
            let mut pen = Pen {
                builder: PathBuilder::new(),
                x: 0.0,
            };
            for ch in text.chars() {
                let glyph = match charmap.map(ch) {
                    Some(glyph) => glyph,
                    None if ch.is_whitespace() => GlyphId::NOTDEF,
                    None => return None,
                };
                if let Some(outline) = outlines.get(glyph) {
                    // A glyph that fails to draw is left out.
                    let _ = outline.draw(
                        DrawSettings::unhinted(size, LocationRef::default()),
                        &mut pen,
                    );
                }
                pen.x += advances
                    .advance_width(glyph)
                    .unwrap_or(if ch.is_whitespace() { px / 4.0 } else { 0.0 });
            }
            Some(pen.builder.finish())
        })?
    }

    /// The regular face of `family` (or the closest to it).
    fn face_id(&self, family: &str) -> Option<fontdb::ID> {
        self.system.db().query(&fontdb::Query {
            families: &[Family::Name(family)],
            weight: Weight::NORMAL,
            stretch: Stretch::Normal,
            style: Style::Normal,
        })
    }

    /// Shapes `text` and returns its glyph outlines as one path, in pixels,
    /// with the first line's baseline at y = 0 and the next ones
    /// `line_height` below. When wrapping, each line is centred on x = 0;
    /// otherwise the line starts at x = 0.
    fn shape(
        &mut self,
        family: &str,
        text: &str,
        font_size: f32,
        line_height: f32,
        wrap_width: Option<f32>,
    ) -> Shaped {
        let mut buffer = Buffer::new(&mut self.system, Metrics::new(font_size, line_height));
        buffer.set_wrap(match wrap_width {
            Some(_) => Wrap::WordOrGlyph,
            None => Wrap::None,
        });
        buffer.set_size(wrap_width, None);
        let attrs = Attrs::new().family(Family::Name(family));
        let align = wrap_width.map_or(Align::Left, |_| Align::Center);
        buffer.set_text(text, &attrs, Shaping::Advanced, Some(align));
        buffer.shape_until_scroll(&mut self.system, false);

        // Centred within the wrap width, so its middle is the line's centre.
        let dx = wrap_width.map_or(0.0, |w| -w / 2.0);
        let mut builder = PathBuilder::new();
        let mut lines = 0;
        for run in buffer.layout_runs() {
            lines += 1;
            let baseline = (run.line_top / line_height).round() * line_height;
            for glyph in run.glyphs {
                // Unhinted and unshifted: the exact position is applied below.
                let key = CacheKey {
                    font_id: glyph.font_id,
                    glyph_id: glyph.glyph_id,
                    font_size_bits: glyph.font_size.to_bits(),
                    x_bin: SubpixelBin::Zero,
                    y_bin: SubpixelBin::Zero,
                    font_weight: glyph.font_weight,
                    flags: glyph.cache_key_flags | CacheKeyFlags::DISABLE_HINTING,
                };
                // Not cached: scaling an outline is cheap, and a cache keyed
                // by size would grow with every frame size and every font
                // shown in the picker.
                let Some(commands) = self
                    .scaler
                    .get_outline_commands_uncached(&mut self.system, key)
                else {
                    continue; // bitmap-only glyphs (some emoji)
                };
                let ox = dx + glyph.x + glyph.font_size * glyph.x_offset;
                let oy = baseline + glyph.y - glyph.font_size * glyph.y_offset;
                // Font outlines point up; the image's y axis points down.
                for command in commands.iter() {
                    match *command {
                        Command::MoveTo(p) => builder.move_to(ox + p.x, oy - p.y),
                        Command::LineTo(p) => builder.line_to(ox + p.x, oy - p.y),
                        Command::QuadTo(c, p) => {
                            builder.quad_to(ox + c.x, oy - c.y, ox + p.x, oy - p.y)
                        }
                        Command::CurveTo(c1, c2, p) => builder.cubic_to(
                            ox + c1.x,
                            oy - c1.y,
                            ox + c2.x,
                            oy - c2.y,
                            ox + p.x,
                            oy - p.y,
                        ),
                        Command::Close => builder.close(),
                    }
                }
            }
        }
        Shaped {
            path: builder.finish(),
            lines,
        }
    }

    /// Height of capitals in `family` at `font_size`, from the font's
    /// metrics (70 % of the ascent when it doesn't say).
    fn cap_height(&mut self, family: &str, font_size: f32) -> f32 {
        let font = self
            .face_id(family)
            .and_then(|id| self.system.get_font(id, Weight::NORMAL));
        let Some(font) = font else {
            return font_size * 0.7;
        };
        let metrics = font.metrics();
        let em = f32::from(metrics.units_per_em.max(1));
        let cap = metrics
            .cap_height
            .filter(|cap| *cap > 0.0)
            .unwrap_or(metrics.ascent * 0.7);
        cap / em * font_size
    }
}

/// Collects a glyph's outline into a path, at `x` on the baseline. Font
/// outlines point up; the image's y axis points down.
struct Pen {
    builder: PathBuilder,
    x: f32,
}

impl OutlinePen for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.builder.move_to(self.x + x, -y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.builder.line_to(self.x + x, -y);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.builder.quad_to(self.x + cx0, -cy0, self.x + x, -y);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let ox = self.x;
        self.builder
            .cubic_to(ox + cx0, -cy0, ox + cx1, -cy1, ox + x, -y);
    }

    fn close(&mut self) {
        self.builder.close();
    }
}

struct Shaped {
    path: Option<Path>,
    lines: usize,
}

/// A rectangle on the pixel grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl PixelRect {
    /// The pixels `rect` touches once grown by `margin` on every side.
    fn around(rect: Rect, margin: f32) -> Option<PixelRect> {
        let x0 = (rect.left() - margin).floor();
        let y0 = (rect.top() - margin).floor();
        let x1 = (rect.right() + margin).ceil();
        let y1 = (rect.bottom() + margin).ceil();
        let (width, height) = (x1 - x0, y1 - y0);
        let valid = [x0, y0, x1, y1].iter().all(|v| v.abs() < 1e7)
            && width >= 1.0
            && height >= 1.0
            && (width as u64) * (height as u64) <= MAX_PIXELS;
        valid.then_some(PixelRect {
            x: x0 as i32,
            y: y0 as i32,
            width: width as u32,
            height: height as u32,
        })
    }

    fn union(self, other: PixelRect) -> PixelRect {
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = (self.x + self.width as i32).max(other.x + other.width as i32);
        let y1 = (self.y + self.height as i32).max(other.y + other.height as i32);
        PixelRect {
            x: x0,
            y: y0,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        }
    }
}

/// A caption drawn for a frame of a given size.
#[derive(Debug, Clone)]
pub struct RenderedCaption {
    /// Just the caption, outline and shadow included. Never empty: a caption
    /// without text gives a 1x1 transparent image at its centre.
    pub image: RgbaImage,
    /// Where its top-left corner goes on the frame; may be off the frame.
    pub x: i32,
    pub y: i32,
    /// The caption's font isn't installed and Impact (or, failing that, any
    /// sans-serif) was used instead.
    pub fell_back: bool,
    /// The family actually drawn (the installed spelling of the caption's
    /// font, or the fallback).
    pub family: String,
}

impl RenderedCaption {
    /// Whether the frame pixel (`x`, `y`) is within the caption's box, for
    /// picking the caption under the mouse.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x
            && y >= self.y
            && i64::from(x) < i64::from(self.x) + i64::from(self.image.width)
            && i64::from(y) < i64::from(self.y) + i64::from(self.image.height)
    }
}

/// Where a caption goes on a frame, before drawing.
struct Placed {
    /// The glyph outlines in frame pixels, or `None` when nothing shows.
    path: Option<Path>,
    rect: PixelRect,
    stroke: Option<(Color, f32)>,
    shadow: Option<(f32, usize)>,
    family: String,
    fell_back: bool,
}

fn place(fonts: &mut Fonts, caption: &Caption, frame_width: u32, frame_height: u32) -> Placed {
    let (family, fell_back) = fonts.resolve_family(&caption.font);
    let family = family.to_owned();
    let (fw, fh) = (frame_width as f32, frame_height as f32);
    let (cx, cy) = (finite(caption.x) * fw, finite(caption.y) * fh);
    let empty = |family: String| Placed {
        path: None,
        rect: PixelRect {
            x: cx.clamp(-1e7, 1e7) as i32,
            y: cy.clamp(-1e7, 1e7) as i32,
            width: 1,
            height: 1,
        },
        stroke: None,
        shadow: None,
        family,
        fell_back,
    };

    let text = caption.display_text();
    let text = text.trim();
    let size = finite(caption.size) * fh;
    if text.is_empty() || size < 0.5 || frame_width == 0 {
        return empty(family);
    }

    // A line is `size` tall, and the font size is the same (CSS's
    // `line-height: 1`), which spaces Impact's capitals like the memes do.
    let wrap = (fw * WRAP_WIDTH).max(size);
    let shaped = fonts.shape(&family, text, size, size, Some(wrap));
    let Some(path) = shaped.path else {
        return empty(family);
    };
    // Centred on the capitals: from the top of the first line's capitals to
    // the last line's baseline, so the caption doesn't move while typing.
    let cap = fonts.cap_height(&family, size);
    let block = cap + (shaped.lines.max(1) - 1) as f32 * size;
    let top = cy - block / 2.0;
    let Some(path) = path.transform(Transform::from_translate(cx, top + cap)) else {
        return empty(family);
    };

    let width = finite(caption.outline_width).max(0.0) * size;
    let stroke = (width > 0.0 && caption.outline.a > 0).then_some((caption.outline, width));
    let shadow = caption.shadow.then(|| {
        let offset = (SHADOW_OFFSET * size).max(1.0);
        let blur = (SHADOW_BLUR * size).round() as usize;
        (offset, blur)
    });

    let Some(bounds) = path.compute_tight_bounds() else {
        return empty(family);
    };
    // One pixel more for anti-aliasing.
    let margin = stroke.map_or(0.0, |(_, w)| w / 2.0) + 1.0;
    let Some(mut rect) = PixelRect::around(bounds, margin) else {
        return empty(family);
    };
    if let Some((offset, blur)) = shadow {
        let moved = Rect::from_ltrb(
            bounds.left() + offset,
            bounds.top() + offset,
            bounds.right() + offset,
            bounds.bottom() + offset,
        )
        // Two box blurs of radius `blur` spread by twice that.
        .and_then(|b| PixelRect::around(b, margin + 2.0 * blur as f32));
        if let Some(moved) = moved {
            rect = rect.union(moved);
        }
    }
    Placed {
        path: Some(path),
        rect,
        stroke,
        shadow,
        family,
        fell_back,
    }
}

fn finite(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

/// Draws `caption` as it shows on a `frame_width` x `frame_height` frame:
/// its size, outline width and position are fractions of the frame.
///
/// The text is centred on (`caption.x`, `caption.y`): horizontally on each
/// line's advance, vertically from the top of the first line's capitals to
/// the last line's baseline. Lines wrap at 90 % of the frame's width.
pub fn render_caption(
    fonts: &mut Fonts,
    caption: &Caption,
    frame_width: u32,
    frame_height: u32,
) -> RenderedCaption {
    let placed = place(fonts, caption, frame_width, frame_height);
    let image = match &placed.path {
        Some(path) => draw::rasterize(
            path,
            placed.rect,
            caption.fill,
            placed.stroke,
            placed.shadow,
        ),
        None => RgbaImage::new(1, 1),
    };
    RenderedCaption {
        image,
        x: placed.rect.x,
        y: placed.rect.y,
        fell_back: placed.fell_back,
        family: placed.family,
    }
}

/// The box [`render_caption`] would return, as (x, y, width, height) on the
/// frame, without drawing: for hit-testing and selection outlines. Costs the
/// layout only.
pub fn caption_rect(
    fonts: &mut Fonts,
    caption: &Caption,
    frame_width: u32,
    frame_height: u32,
) -> (i32, i32, u32, u32) {
    let rect = place(fonts, caption, frame_width, frame_height).rect;
    (rect.x, rect.y, rect.width, rect.height)
}

/// Draws `image` onto `frame` with its top-left corner at (`x`, `y`),
/// blending by alpha. Both are straight RGBA; whatever falls outside the
/// frame is clipped.
pub fn blend(frame: &mut RgbaImage, image: &RgbaImage, x: i32, y: i32) {
    draw::blend(frame, image, x, y);
}

/// The captions as an [`Overlay`], for exports: each caption is drawn once
/// per frame size and reused while it shows.
pub struct CaptionOverlay {
    fonts: Arc<Mutex<Fonts>>,
    captions: Vec<Cached>,
    cut: Cut,
    /// How many times a caption was drawn, for the tests.
    renders: usize,
}

struct Cached {
    caption: Caption,
    /// The last render and the frame size it was for.
    render: Option<((u32, u32), RenderedCaption)>,
}

impl CaptionOverlay {
    pub fn new(fonts: Arc<Mutex<Fonts>>, captions: Vec<Caption>, cut: Cut) -> CaptionOverlay {
        let captions = captions
            .into_iter()
            .filter(|caption| !caption.text.trim().is_empty())
            .map(|caption| Cached {
                caption,
                render: None,
            })
            .collect();
        CaptionOverlay {
            fonts,
            captions,
            cut,
            renders: 0,
        }
    }
}

impl Overlay for CaptionOverlay {
    fn draw(&mut self, t: f64, frame: &mut RgbaImage) {
        let size = (frame.width, frame.height);
        for cached in &mut self.captions {
            if !cached.caption.visible_at(t, self.cut) {
                continue;
            }
            let stale = cached.render.as_ref().is_none_or(|(s, _)| *s != size);
            if stale {
                let mut fonts = self.fonts.lock().unwrap_or_else(PoisonError::into_inner);
                let rendered = render_caption(&mut fonts, &cached.caption, size.0, size.1);
                cached.render = Some((size, rendered));
                self.renders += 1;
            }
            if let Some((_, r)) = &cached.render {
                blend(frame, &r.image, r.x, r.y);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.captions.is_empty()
    }
}
