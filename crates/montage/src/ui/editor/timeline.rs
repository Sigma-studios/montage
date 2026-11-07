//! The timeline: a strip with the captions' timing, the filmstrip, the mixed
//! waveform, the two cut handles with the outside of the cut dimmed, and the
//! playhead. Click or drag to scrub; drag a handle to move it.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind,
    TextureHandle, Vec2,
};
use montage_core::edit::{Caption, Cut};

use super::style;
use super::thumbs::Loadable;

/// The shortest cut, and the shortest caption timing, in seconds.
pub const MIN_CUT: f64 = 0.1;

/// Room left and right of the clip for the cut handles, in points.
const PAD: f32 = 12.0;
const HANDLE_WIDTH: f32 = 9.0;
/// How far from a handle a press still grabs it, in points.
const GRAB: f32 = 9.0;
const CAPTION_STRIP: f32 = 16.0;
const FILM_HEIGHT: f32 = 54.0;
const WAVE_HEIGHT: f32 = 40.0;
const ROW_GAP: f32 = 3.0;

/// Clamps without panicking when `lo > hi` (a clip shorter than
/// [`MIN_CUT`]): `lo` wins.
fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    v.min(hi).max(lo)
}

/// Maps the clip's time to x positions on the timeline and back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeMap {
    pub x0: f32,
    pub x1: f32,
    pub duration: f64,
}

impl TimeMap {
    pub fn x(&self, t: f64) -> f32 {
        if self.duration <= 0.0 {
            return self.x0;
        }
        let f = (t / self.duration).clamp(0.0, 1.0) as f32;
        self.x0 + f * (self.x1 - self.x0)
    }

    pub fn t(&self, x: f32) -> f64 {
        let width = self.x1 - self.x0;
        if width <= 0.0 {
            return 0.0;
        }
        let f = ((x - self.x0) / width).clamp(0.0, 1.0);
        f64::from(f) * self.duration
    }
}

/// The start handle moved to `t`: kept before the end.
pub fn drag_start(cut: Cut, t: f64, _duration: f64) -> Cut {
    Cut {
        start: clamp(t, 0.0, cut.end - MIN_CUT),
        end: cut.end,
    }
}

/// The end handle moved to `t`: kept after the start, within the clip.
pub fn drag_end(cut: Cut, t: f64, duration: f64) -> Cut {
    Cut {
        start: cut.start,
        end: clamp(t, cut.start + MIN_CUT, duration),
    }
}

/// `I`: the cut starts at `t`. Past the end, the end goes back to the end
/// of the clip rather than refusing.
pub fn set_in(cut: Cut, t: f64, duration: f64) -> Cut {
    let t = clamp(t, 0.0, duration);
    if t <= cut.end - MIN_CUT {
        return Cut {
            start: t,
            end: cut.end,
        };
    }
    let start = clamp(t, 0.0, duration - MIN_CUT);
    Cut {
        start,
        end: duration,
    }
}

/// `O`: the cut ends at `t`. Before the start, the start goes back to the
/// start of the clip.
pub fn set_out(cut: Cut, t: f64, duration: f64) -> Cut {
    let t = clamp(t, 0.0, duration);
    if t >= cut.start + MIN_CUT {
        return Cut {
            start: cut.start,
            end: t,
        };
    }
    Cut {
        start: 0.0,
        end: clamp(t, MIN_CUT.min(duration), duration),
    }
}

/// The time a caption shows, its own or the cut's.
pub fn caption_range(caption: &Caption, cut: Cut) -> (f64, f64) {
    (
        caption.start.unwrap_or(cut.start),
        caption.end.unwrap_or(cut.end),
    )
}

/// A caption's start handle moved to `t`.
pub fn drag_caption_start(caption: &mut Caption, t: f64, cut: Cut) {
    let (_, end) = caption_range(caption, cut);
    caption.start = Some(clamp(t, 0.0, end - MIN_CUT));
}

/// A caption's end handle moved to `t`.
pub fn drag_caption_end(caption: &mut Caption, t: f64, cut: Cut, duration: f64) {
    let (start, _) = caption_range(caption, cut);
    caption.end = Some(clamp(t, start + MIN_CUT, duration));
}

/// What a press on the timeline grabbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragTarget {
    Scrub,
    CutStart,
    CutEnd,
    CaptionStart,
    CaptionEnd,
}

/// Where the start handle's middle is: just left of the cut.
fn start_handle_x(map: &TimeMap, cut: Cut) -> f32 {
    map.x(cut.start) - HANDLE_WIDTH / 2.0
}

fn end_handle_x(map: &TimeMap, cut: Cut) -> f32 {
    map.x(cut.end) + HANDLE_WIDTH / 2.0
}

/// What a press at `x` grabs. `in_caption_strip` is true when it's in the
/// row of caption timings, where the selected caption's handles are.
pub fn hit_test(
    x: f32,
    in_caption_strip: bool,
    map: &TimeMap,
    cut: Cut,
    caption: Option<(f64, f64)>,
) -> DragTarget {
    let closest = |a: (f32, DragTarget), b: (f32, DragTarget)| {
        let (da, db) = ((x - a.0).abs(), (x - b.0).abs());
        let best = if da <= db { (da, a.1) } else { (db, b.1) };
        (best.0 <= GRAB).then_some(best.1)
    };
    if in_caption_strip && let Some((start, end)) = caption {
        let hit = closest(
            (map.x(start), DragTarget::CaptionStart),
            (map.x(end), DragTarget::CaptionEnd),
        );
        if let Some(hit) = hit {
            return hit;
        }
    }
    closest(
        (start_handle_x(map, cut), DragTarget::CutStart),
        (end_handle_x(map, cut), DragTarget::CutEnd),
    )
    .unwrap_or(DragTarget::Scrub)
}

/// The part of a `tex_aspect` image shown in a `slot_aspect` slot, cropped
/// around the middle so it isn't stretched.
pub fn crop_uv(tex_aspect: f32, slot_aspect: f32) -> Rect {
    if tex_aspect <= 0.0 || slot_aspect <= 0.0 {
        return Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    }
    if tex_aspect > slot_aspect {
        let w = slot_aspect / tex_aspect;
        Rect::from_min_max(Pos2::new(0.5 - w / 2.0, 0.0), Pos2::new(0.5 + w / 2.0, 1.0))
    } else {
        let h = tex_aspect / slot_aspect;
        Rect::from_min_max(Pos2::new(0.0, 0.5 - h / 2.0), Pos2::new(1.0, 0.5 + h / 2.0))
    }
}

/// `m:ss.cc`.
pub fn format_time(t: f64) -> String {
    let t = t.max(0.0);
    let centis = (t * 100.0).round() as u64;
    let (minutes, centis) = (centis / 6000, centis % 6000);
    format!("{minutes}:{:02}.{:02}", centis / 100, centis % 100)
}

/// The waveform, as far as it's known.
pub enum WaveInput<'a> {
    Loading,
    Failed(&'a str),
    /// The mixed peaks and the seconds each covers.
    Ready {
        peaks: &'a [(f32, f32)],
        bucket_seconds: f64,
        /// Drawn this many times larger (clipped to the row).
        scale: f32,
    },
}

/// What the timeline draws.
pub struct TimelineInput<'a> {
    pub duration: f64,
    pub position: f64,
    pub filmstrip: &'a Loadable<Vec<TextureHandle>>,
    pub waveform: WaveInput<'a>,
    pub selected_caption: Option<usize>,
}

#[derive(Debug, Default)]
pub struct TimelineOutput {
    /// Show this time: the user scrubbed or moved a handle.
    pub seek: Option<f64>,
    /// A caption's timing strip was pressed: select it.
    pub select_caption: Option<usize>,
}

pub fn height() -> f32 {
    CAPTION_STRIP + ROW_GAP + FILM_HEIGHT + ROW_GAP + WAVE_HEIGHT
}

pub fn show(
    ui: &mut egui::Ui,
    drag: &mut Option<DragTarget>,
    input: TimelineInput,
    cut: &mut Cut,
    captions: &mut [Caption],
) -> TimelineOutput {
    let mut out = TimelineOutput::default();
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), height()),
        Sense::click_and_drag(),
    );
    let map = TimeMap {
        x0: rect.left() + PAD,
        x1: rect.right() - PAD,
        duration: input.duration,
    };
    let strip = Rect::from_min_max(
        Pos2::new(map.x0, rect.top()),
        Pos2::new(map.x1, rect.top() + CAPTION_STRIP),
    );
    let film = Rect::from_min_size(
        Pos2::new(map.x0, strip.bottom() + ROW_GAP),
        Vec2::new(map.x1 - map.x0, FILM_HEIGHT),
    );
    let wave = Rect::from_min_size(
        Pos2::new(map.x0, film.bottom() + ROW_GAP),
        Vec2::new(map.x1 - map.x0, WAVE_HEIGHT),
    );
    let selected = input
        .selected_caption
        .and_then(|i| captions.get(i).map(|c| caption_range(c, *cut)));

    // Interaction first, so this frame draws the result.
    if (response.drag_started() || response.clicked())
        && let Some(pos) = response.interact_pointer_pos()
    {
        let in_strip = pos.y < strip.bottom() + ROW_GAP / 2.0;
        let target = hit_test(pos.x, in_strip, &map, *cut, selected);
        if target == DragTarget::Scrub && in_strip {
            let t = map.t(pos.x);
            // A press on another caption's bar selects it.
            if let Some(i) = captions.iter().position(|c| {
                let (s, e) = caption_range(c, *cut);
                t >= s && t <= e
            }) && Some(i) != input.selected_caption
            {
                out.select_caption = Some(i);
            }
        }
        *drag = Some(target);
    }
    if let (Some(target), Some(pos)) = (*drag, response.interact_pointer_pos())
        && (response.dragged() || response.drag_started() || response.clicked())
    {
        let t = map.t(pos.x);
        match target {
            DragTarget::Scrub => out.seek = Some(t),
            DragTarget::CutStart => {
                *cut = drag_start(*cut, t, input.duration);
                out.seek = Some(cut.start);
            }
            DragTarget::CutEnd => {
                *cut = drag_end(*cut, t, input.duration);
                out.seek = Some(cut.end);
            }
            DragTarget::CaptionStart | DragTarget::CaptionEnd => {
                if let Some(caption) = input.selected_caption.and_then(|i| captions.get_mut(i)) {
                    if target == DragTarget::CaptionStart {
                        drag_caption_start(caption, t, *cut);
                        out.seek = caption.start;
                    } else {
                        drag_caption_end(caption, t, *cut, input.duration);
                        out.seek = caption.end;
                    }
                }
            }
        }
    }
    if response.drag_stopped() || response.clicked() {
        *drag = None;
    }
    if let Some(pos) = response.hover_pos() {
        let in_strip = pos.y < strip.bottom() + ROW_GAP / 2.0;
        let over = drag.unwrap_or_else(|| hit_test(pos.x, in_strip, &map, *cut, selected));
        if over != DragTarget::Scrub {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
    }

    let painter = ui.painter_at(rect.expand(2.0));
    let selected = input
        .selected_caption
        .and_then(|i| captions.get(i).map(|c| caption_range(c, *cut)));

    // Caption timings.
    painter.rect_filled(strip, CornerRadius::same(4), style::PANEL);
    // The selected caption last, on top of the others.
    let mut order: Vec<usize> = (0..captions.len()).collect();
    order.sort_by_key(|&i| Some(i) == input.selected_caption);
    for i in order {
        let (s, e) = caption_range(&captions[i], *cut);
        let bar = Rect::from_min_max(
            Pos2::new(map.x(s), strip.center().y - 3.0),
            Pos2::new(map.x(e).max(map.x(s) + 2.0), strip.center().y + 3.0),
        );
        let color = if Some(i) == input.selected_caption {
            style::ACCENT
        } else {
            style::SUBTEXT.gamma_multiply(0.5)
        };
        painter.rect_filled(bar, CornerRadius::same(3), color);
    }
    if let Some((s, e)) = selected {
        for t in [s, e] {
            painter.circle(
                Pos2::new(map.x(t), strip.center().y),
                5.0,
                style::ACCENT,
                Stroke::new(1.5, style::PANEL),
            );
        }
    }

    // Filmstrip.
    painter.rect_filled(film, CornerRadius::same(4), style::INPUT);
    match input.filmstrip {
        Loadable::Ready(frames) if !frames.is_empty() => {
            let slot_w = film.width() / frames.len() as f32;
            for (i, tex) in frames.iter().enumerate() {
                let slot = Rect::from_min_size(
                    Pos2::new(film.left() + i as f32 * slot_w, film.top()),
                    Vec2::new(slot_w, film.height()),
                );
                let [w, h] = tex.size();
                let uv = crop_uv(w as f32 / h.max(1) as f32, slot.aspect_ratio());
                painter.image(tex.id(), slot, uv, Color32::WHITE);
            }
        }
        Loadable::Failed(e) => placeholder(&painter, film, &format!("No thumbnails: {e}")),
        _ => placeholder(&painter, film, "Making thumbnails…"),
    }

    // Waveform.
    painter.rect_filled(wave, CornerRadius::same(4), style::INPUT);
    match input.waveform {
        WaveInput::Ready {
            peaks,
            bucket_seconds,
            scale,
        } if !peaks.is_empty() && bucket_seconds > 0.0 => {
            draw_waveform(&painter, wave, &map, peaks, bucket_seconds, scale);
        }
        WaveInput::Ready { .. } => placeholder(&painter, wave, "No audio"),
        WaveInput::Failed(e) => placeholder(&painter, wave, &format!("No waveform: {e}")),
        WaveInput::Loading => placeholder(&painter, wave, "Reading the audio…"),
    }

    // Outside the cut, dimmed.
    let body = Rect::from_min_max(film.min, wave.max);
    let (cx0, cx1) = (map.x(cut.start), map.x(cut.end));
    let dim = Color32::from_black_alpha(170);
    if cx0 > body.left() {
        painter.rect_filled(
            Rect::from_min_max(body.min, Pos2::new(cx0, body.bottom())),
            CornerRadius::ZERO,
            dim,
        );
    }
    if cx1 < body.right() {
        painter.rect_filled(
            Rect::from_min_max(Pos2::new(cx1, body.top()), body.max),
            CornerRadius::ZERO,
            dim,
        );
    }

    // The cut's frame and handles.
    let frame = Rect::from_min_max(Pos2::new(cx0, body.top()), Pos2::new(cx1, body.bottom()));
    painter.rect_stroke(
        frame,
        CornerRadius::ZERO,
        Stroke::new(2.0, style::ACCENT),
        StrokeKind::Outside,
    );
    for (x, rounding) in [
        (
            start_handle_x(&map, *cut),
            CornerRadius {
                nw: 5,
                sw: 5,
                ne: 0,
                se: 0,
            },
        ),
        (
            end_handle_x(&map, *cut),
            CornerRadius {
                nw: 0,
                sw: 0,
                ne: 5,
                se: 5,
            },
        ),
    ] {
        let handle = Rect::from_center_size(
            Pos2::new(x, body.center().y),
            Vec2::new(HANDLE_WIDTH, body.height() + 4.0),
        );
        painter.rect_filled(handle, rounding, style::ACCENT);
        painter.line_segment(
            [
                Pos2::new(x, body.center().y - 7.0),
                Pos2::new(x, body.center().y + 7.0),
            ],
            Stroke::new(1.5, Color32::from_black_alpha(140)),
        );
    }

    // Playhead.
    let px = map.x(input.position);
    painter.line_segment(
        [
            Pos2::new(px, strip.top()),
            Pos2::new(px, body.bottom() + 2.0),
        ],
        Stroke::new(2.0, Color32::WHITE),
    );
    painter.circle_filled(Pos2::new(px, strip.top() + 3.0), 4.0, Color32::WHITE);

    out
}

fn placeholder(painter: &egui::Painter, rect: Rect, text: &str) {
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(12.0),
        style::SUBTEXT,
    );
}

fn draw_waveform(
    painter: &egui::Painter,
    rect: Rect,
    map: &TimeMap,
    peaks: &[(f32, f32)],
    bucket_seconds: f64,
    scale: f32,
) {
    let mid = rect.center().y;
    let half = rect.height() / 2.0 - 2.0;
    painter.line_segment(
        [Pos2::new(rect.left(), mid), Pos2::new(rect.right(), mid)],
        Stroke::new(1.0, style::WAVEFORM.gamma_multiply(0.35)),
    );
    let stroke = Stroke::new(1.0, style::WAVEFORM);
    let mut x = rect.left();
    while x < rect.right() {
        let b0 = (map.t(x) / bucket_seconds) as usize;
        let b1 = ((map.t(x + 1.0) / bucket_seconds).ceil() as usize).max(b0 + 1);
        let (lo, hi) = peaks[b0.min(peaks.len())..b1.min(peaks.len())]
            .iter()
            .fold((0.0f32, 0.0f32), |(lo, hi), &(min, max)| {
                (lo.min(min), hi.max(max))
            });
        if hi > lo {
            let top = mid - (hi * scale).min(1.0) * half;
            let bottom = (mid - (lo * scale).max(-1.0) * half).max(top + 1.0);
            painter.line_segment(
                [Pos2::new(x + 0.5, top), Pos2::new(x + 0.5, bottom)],
                stroke,
            );
        }
        x += 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAP: TimeMap = TimeMap {
        x0: 100.0,
        x1: 1100.0,
        duration: 50.0,
    };

    fn cut(start: f64, end: f64) -> Cut {
        Cut { start, end }
    }

    #[test]
    fn time_maps_to_x_and_back() {
        assert_eq!(MAP.x(0.0), 100.0);
        assert_eq!(MAP.x(25.0), 600.0);
        assert_eq!(MAP.x(50.0), 1100.0);
        assert_eq!(MAP.x(-3.0), 100.0);
        assert_eq!(MAP.x(80.0), 1100.0);
        assert_eq!(MAP.t(600.0), 25.0);
        assert_eq!(MAP.t(0.0), 0.0);
        assert_eq!(MAP.t(5000.0), 50.0);
        for t in [0.0, 1.5, 12.25, 49.0] {
            assert!((MAP.t(MAP.x(t)) - t).abs() < 1e-3, "{t}");
        }
        let empty = TimeMap {
            x0: 10.0,
            x1: 10.0,
            duration: 0.0,
        };
        assert_eq!(empty.x(3.0), 10.0);
        assert_eq!(empty.t(30.0), 0.0);
    }

    #[test]
    fn cut_handles_stay_apart_and_in_the_clip() {
        let c = cut(10.0, 20.0);
        assert_eq!(drag_start(c, 5.0, 50.0), cut(5.0, 20.0));
        assert_eq!(drag_start(c, -5.0, 50.0), cut(0.0, 20.0));
        assert_eq!(drag_start(c, 30.0, 50.0), cut(20.0 - MIN_CUT, 20.0));
        assert_eq!(drag_end(c, 30.0, 50.0), cut(10.0, 30.0));
        assert_eq!(drag_end(c, 99.0, 50.0), cut(10.0, 50.0));
        assert_eq!(drag_end(c, 2.0, 50.0), cut(10.0, 10.0 + MIN_CUT));
        // A clip shorter than the minimum doesn't panic.
        let tiny = cut(0.0, 0.05);
        assert_eq!(drag_end(tiny, 0.01, 0.05).start, 0.0);
    }

    #[test]
    fn in_and_out_points_move_the_other_end_when_crossed() {
        let c = cut(10.0, 20.0);
        assert_eq!(set_in(c, 15.0, 50.0), cut(15.0, 20.0));
        assert_eq!(set_in(c, 25.0, 50.0), cut(25.0, 50.0));
        assert_eq!(set_in(c, 50.0, 50.0), cut(50.0 - MIN_CUT, 50.0));
        assert_eq!(set_out(c, 15.0, 50.0), cut(10.0, 15.0));
        assert_eq!(set_out(c, 5.0, 50.0), cut(0.0, 5.0));
        assert_eq!(set_out(c, 0.0, 50.0), cut(0.0, MIN_CUT));
    }

    #[test]
    fn caption_handles_stay_ordered() {
        let c = cut(2.0, 12.0);
        let mut caption = Caption::default();
        assert_eq!(caption_range(&caption, c), (2.0, 12.0));
        drag_caption_start(&mut caption, 4.0, c);
        assert_eq!(caption.start, Some(4.0));
        drag_caption_end(&mut caption, 3.0, c, 30.0);
        assert_eq!(caption.end, Some(4.0 + MIN_CUT));
        drag_caption_end(&mut caption, 99.0, c, 30.0);
        assert_eq!(caption.end, Some(30.0));
        drag_caption_start(&mut caption, 40.0, c);
        assert_eq!(caption.start, Some(30.0 - MIN_CUT));
    }

    #[test]
    fn presses_grab_the_nearest_handle() {
        let c = cut(10.0, 20.0); // x 300 and 500
        let start = 300.0 - HANDLE_WIDTH / 2.0;
        let end = 500.0 + HANDLE_WIDTH / 2.0;
        assert_eq!(hit_test(start, false, &MAP, c, None), DragTarget::CutStart);
        assert_eq!(
            hit_test(start + 5.0, false, &MAP, c, None),
            DragTarget::CutStart
        );
        assert_eq!(
            hit_test(end - 3.0, false, &MAP, c, None),
            DragTarget::CutEnd
        );
        assert_eq!(hit_test(400.0, false, &MAP, c, None), DragTarget::Scrub);
        // Handles of a very short cut: the closer one wins.
        let short = cut(10.0, 10.2); // x 300 and 304
        assert_eq!(
            hit_test(294.0, false, &MAP, short, None),
            DragTarget::CutStart
        );
        assert_eq!(
            hit_test(310.0, false, &MAP, short, None),
            DragTarget::CutEnd
        );
        // Caption handles only in their strip.
        let caption = Some((15.0, 18.0)); // x 400 and 460
        assert_eq!(
            hit_test(402.0, true, &MAP, c, caption),
            DragTarget::CaptionStart
        );
        assert_eq!(
            hit_test(458.0, true, &MAP, c, caption),
            DragTarget::CaptionEnd
        );
        assert_eq!(hit_test(402.0, false, &MAP, c, caption), DragTarget::Scrub);
        assert_eq!(hit_test(430.0, true, &MAP, c, caption), DragTarget::Scrub);
    }

    #[test]
    fn thumbnails_are_cropped_not_stretched() {
        // A 16:9 frame in a 1:1 slot keeps its middle 9/16 of the width.
        let uv = crop_uv(16.0 / 9.0, 1.0);
        assert!((uv.width() - 9.0 / 16.0).abs() < 1e-6);
        assert!((uv.center().x - 0.5).abs() < 1e-6);
        assert_eq!(uv.height(), 1.0);
        // A 1:1 frame in a 2:1 slot keeps the middle half of the height.
        let uv = crop_uv(1.0, 2.0);
        assert_eq!(uv.width(), 1.0);
        assert!((uv.height() - 0.5).abs() < 1e-6);
        assert_eq!(crop_uv(0.0, 1.0).width(), 1.0);
    }

    #[test]
    fn times_print_as_minutes_seconds_hundredths() {
        assert_eq!(format_time(0.0), "0:00.00");
        assert_eq!(format_time(5.256), "0:05.26");
        assert_eq!(format_time(65.5), "1:05.50");
        assert_eq!(format_time(-1.0), "0:00.00");
        assert_eq!(format_time(59.999), "1:00.00");
    }
}
