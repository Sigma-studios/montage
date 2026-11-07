//! Pixels: filling and stroking the caption's path, the shadow's blur, and
//! blending onto a frame.

use montage_core::edit::Color;
use montage_core::render::RgbaImage;
use tiny_skia::{
    FillRule, LineCap, LineJoin, Paint, Path, Pixmap, PremultipliedColorU8, Stroke, Transform,
};

use crate::{PixelRect, SHADOW_ALPHA};

/// Draws `path` (in frame pixels) into an image covering `rect`: the shadow
/// (offset, blur radius) first, then the outline (colour, full stroke
/// width), then the fill on top.
pub(crate) fn rasterize(
    path: &Path,
    rect: PixelRect,
    fill: Color,
    stroke: Option<(Color, f32)>,
    shadow: Option<(f32, usize)>,
) -> RgbaImage {
    let Some(mut pixmap) = Pixmap::new(rect.width, rect.height) else {
        return RgbaImage::new(1, 1);
    };
    let origin = Transform::from_translate(-rect.x as f32, -rect.y as f32);
    let stroke_style = stroke.map(|(_, width)| Stroke {
        width,
        line_join: LineJoin::Round,
        line_cap: LineCap::Round,
        ..Stroke::default()
    });

    if let Some((offset, blur)) = shadow {
        let mut mask = Pixmap::new(rect.width, rect.height).expect("same size as above");
        let moved = origin.pre_translate(offset, offset);
        let paint = paint(Color::BLACK);
        mask.fill_path(path, &paint, FillRule::Winding, moved, None);
        if let Some(style) = &stroke_style {
            mask.stroke_path(path, &paint, style, moved, None);
        }
        let mut alpha: Vec<u8> = mask.pixels().iter().map(|p| p.alpha()).collect();
        let (w, h) = (rect.width as usize, rect.height as usize);
        // Two box blurs look close enough to a gaussian.
        box_blur(&mut alpha, w, h, blur);
        box_blur(&mut alpha, w, h, blur);
        for (dst, a) in pixmap.pixels_mut().iter_mut().zip(alpha) {
            let a = (u32::from(a) * u32::from(SHADOW_ALPHA) + 127) / 255;
            *dst = PremultipliedColorU8::from_rgba(0, 0, 0, a as u8).expect("black is valid");
        }
    }
    if let (Some((color, _)), Some(style)) = (stroke, &stroke_style) {
        pixmap.stroke_path(path, &paint(color), style, origin, None);
    }
    pixmap.fill_path(path, &paint(fill), FillRule::Winding, origin, None);

    let mut data = Vec::with_capacity(pixmap.pixels().len() * 4);
    for pixel in pixmap.pixels() {
        let c = pixel.demultiply();
        data.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    RgbaImage {
        width: rect.width,
        height: rect.height,
        data,
    }
}

fn paint(color: Color) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.r, color.g, color.b, color.a);
    paint.anti_alias = true;
    paint
}

/// Averages each value with its neighbours up to `radius` away, across then
/// down. Values past the edges count as zero.
fn box_blur(values: &mut [u8], width: usize, height: usize, radius: usize) {
    if radius == 0 || width == 0 || height == 0 {
        return;
    }
    let mut line = Vec::new();
    for y in 0..height {
        line.clear();
        line.extend((0..width).map(|x| values[y * width + x]));
        blur_line(&line, radius, |x, v| values[y * width + x] = v);
    }
    for x in 0..width {
        line.clear();
        line.extend((0..height).map(|y| values[y * width + x]));
        blur_line(&line, radius, |y, v| values[y * width + x] = v);
    }
}

fn blur_line(line: &[u8], radius: usize, mut set: impl FnMut(usize, u8)) {
    let window = (2 * radius + 1) as u32;
    let n = line.len();
    let mut sum: u32 = line.iter().take(radius + 1).map(|&v| u32::from(v)).sum();
    for i in 0..n {
        set(i, ((sum + window / 2) / window) as u8);
        if i + radius + 1 < n {
            sum += u32::from(line[i + radius + 1]);
        }
        if i >= radius {
            sum -= u32::from(line[i - radius]);
        }
    }
}

/// Straight-alpha "over": `image` onto `frame` at (`x`, `y`), clipped.
pub(crate) fn blend(frame: &mut RgbaImage, image: &RgbaImage, x: i32, y: i32) {
    let (fw, fh) = (i64::from(frame.width), i64::from(frame.height));
    let (iw, ih) = (i64::from(image.width), i64::from(image.height));
    if frame.data.len() < (fw * fh * 4) as usize || image.data.len() < (iw * ih * 4) as usize {
        return; // malformed
    }
    let (x, y) = (i64::from(x), i64::from(y));
    let (x0, x1) = (x.max(0), (x + iw).min(fw));
    let (y0, y1) = (y.max(0), (y + ih).min(fh));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    for fy in y0..y1 {
        let iy = fy - y;
        let src = &image.data[((iy * iw + x0 - x) * 4) as usize..((iy * iw + x1 - x) * 4) as usize];
        let dst = &mut frame.data[((fy * fw + x0) * 4) as usize..((fy * fw + x1) * 4) as usize];
        for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            blend_pixel(d, s);
        }
    }
}

fn blend_pixel(d: &mut [u8], s: &[u8]) {
    let a = u32::from(s[3]);
    if a == 0 {
        return;
    }
    if a == 255 {
        d.copy_from_slice(s);
        return;
    }
    let da = u32::from(d[3]);
    if da == 255 {
        // The usual case: opaque video.
        for i in 0..3 {
            d[i] = ((u32::from(s[i]) * a + u32::from(d[i]) * (255 - a) + 127) / 255) as u8;
        }
        return;
    }
    // Both translucent: weights scaled by 255 * 255.
    let src_w = a * 255;
    let dst_w = da * (255 - a);
    let out = src_w + dst_w;
    if out == 0 {
        d.fill(0);
        return;
    }
    for i in 0..3 {
        d[i] = ((u32::from(s[i]) * src_w + u32::from(d[i]) * dst_w + out / 2) / out) as u8;
    }
    d[3] = ((out + 127) / 255) as u8;
}
