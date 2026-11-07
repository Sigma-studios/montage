//! The library: clips newest first, grouped by day, with thumbnails, stars,
//! delete to the trash, and the total size.
//!
//! Reading the folders and making thumbnails happen on threads; the view
//! only draws what they found. It rescans when it opens, on Refresh, when
//! the window gets the focus back, after a change, and every few seconds.

mod thumbs;

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Rect, Sense, Stroke, Vec2};
use montage_core::Clip;
use montage_core::library::{self, Day, LibraryClip};

use super::{Nav, Shared};
use thumbs::{LoadFn, Loader, Media};

/// How often the folders are read again while the window is open.
const RESCAN_EVERY: Duration = Duration::from_secs(10);

/// Threads making thumbnails. Decoding one frame is mostly waiting on the
/// disk and the decoder's setup, so a few at once keep the tiles coming
/// without taking the machine.
const WORKERS: usize = 3;

const TILE_WIDTH: f32 = 232.0;
const THUMB_SIZE: Vec2 = Vec2::new(TILE_WIDTH, TILE_WIDTH * 9.0 / 16.0);
const TILE_HEIGHT: f32 = THUMB_SIZE.y + 50.0;

const STAR: Color32 = Color32::from_rgb(250, 196, 40);
const EDIT_BADGE: Color32 = Color32::from_rgb(70, 130, 220);
const EXPORT_BADGE: Color32 = Color32::from_rgb(60, 160, 110);

/// A tile's thumbnail and length, once a worker made them.
struct TileMedia {
    texture: Option<egui::TextureHandle>,
    duration: Option<f64>,
    failed: bool,
}

/// What a tile was asked to do this frame.
enum TileAction {
    Open,
    ToggleSelected,
    Star(bool),
}

pub struct LibraryView {
    days: Vec<Day>,
    /// A scan finished at least once, so an empty list means no clips.
    scanned: bool,
    scanning: Option<Receiver<Vec<Day>>>,
    last_scan: Instant,
    was_focused: bool,
    loader: Loader,
    media: HashMap<PathBuf, TileMedia>,
    /// What was last asked of the loader, to ask only on changes.
    wanted: Vec<PathBuf>,
    /// Originals of the selected clips.
    selected: BTreeSet<PathBuf>,
    /// The clips the confirmation dialog would delete.
    confirm_delete: Option<Vec<LibraryClip>>,
    error: Option<String>,
}

impl LibraryView {
    pub fn new(ctx: &egui::Context, shared: &mut Shared) -> LibraryView {
        LibraryView::with_loader(ctx, shared, Arc::new(thumbs::load_media))
    }

    fn with_loader(ctx: &egui::Context, shared: &mut Shared, load: LoadFn) -> LibraryView {
        let mut view = LibraryView {
            days: Vec::new(),
            scanned: false,
            scanning: None,
            last_scan: Instant::now(),
            was_focused: true,
            loader: Loader::new(ctx, WORKERS, load),
            media: HashMap::new(),
            wanted: Vec::new(),
            selected: BTreeSet::new(),
            confirm_delete: None,
            error: None,
        };
        view.rescan(ctx, shared);
        view
    }

    /// Reads the library folders again, on a thread.
    fn rescan(&mut self, ctx: &egui::Context, shared: &Shared) {
        let folders = shared.config.library_folders();
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let days = library::scan(&folders);
            let _ = tx.send(days);
            ctx.request_repaint();
        });
        self.scanning = Some(rx);
        self.last_scan = Instant::now();
    }

    /// Takes in what the threads finished.
    fn receive(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.scanning {
            match rx.try_recv() {
                Ok(days) => {
                    self.days = days;
                    self.scanned = true;
                    self.scanning = None;
                    let listed: BTreeSet<&PathBuf> = self
                        .days
                        .iter()
                        .flat_map(|d| &d.clips)
                        .map(|c| &c.clip.original)
                        .collect();
                    self.selected.retain(|p| listed.contains(p));
                    self.media.retain(|p, _| listed.contains(p));
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.scanning = None,
            }
        }
        for (path, media) in self.loader.finished() {
            let Media {
                image,
                duration,
                error,
            } = media;
            let texture = image.filter(|i| i.width > 0 && i.height > 0).map(|i| {
                let size = [i.width as usize, i.height as usize];
                let color = egui::ColorImage::from_rgba_unmultiplied(size, &i.data);
                ctx.load_texture(
                    format!("thumb:{}", path.display()),
                    color,
                    egui::TextureOptions::LINEAR,
                )
            });
            let failed = texture.is_none() && error.is_some();
            self.media.insert(
                path,
                TileMedia {
                    texture,
                    duration,
                    failed,
                },
            );
        }
    }

    fn all_clips(&self) -> impl Iterator<Item = &LibraryClip> {
        self.days.iter().flat_map(|d| &d.clips)
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, shared: &mut Shared) -> Option<Nav> {
        let ctx = ui.ctx().clone();
        self.receive(&ctx);

        // Rescan on focus, and now and then.
        let focused = ui.input(|i| i.focused);
        let refocused = focused && !self.was_focused;
        self.was_focused = focused;
        if self.scanning.is_none() && (refocused || self.last_scan.elapsed() >= RESCAN_EVERY) {
            self.rescan(&ctx, shared);
        }
        ctx.request_repaint_after(RESCAN_EVERY);

        let mut nav = None;
        if self.confirm_delete.is_none() {
            self.keys(ui);
        }

        egui::Panel::top("library-header").show(ui, |ui| {
            if let Some(n) = self.header(ui, shared) {
                nav = Some(n);
            }
        });
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(n) = self.body(ui, shared) {
                nav = Some(n);
            }
        });
        self.delete_dialog(&ctx, shared);
        nav
    }

    fn keys(&mut self, ui: &egui::Ui) {
        let (delete, escape, select_all) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::Delete)
                    || (i.modifiers.command && i.key_pressed(egui::Key::Backspace)),
                i.key_pressed(egui::Key::Escape),
                i.modifiers.command && i.key_pressed(egui::Key::A),
            )
        });
        if delete && !self.selected.is_empty() {
            self.ask_delete();
        }
        if escape {
            self.selected.clear();
        }
        if select_all && !ui.ctx().egui_wants_keyboard_input() {
            self.selected = self.all_clips().map(|c| c.clip.original.clone()).collect();
        }
    }

    fn ask_delete(&mut self) {
        let clips: Vec<LibraryClip> = self
            .all_clips()
            .filter(|c| self.selected.contains(&c.clip.original))
            .cloned()
            .collect();
        if !clips.is_empty() {
            self.confirm_delete = Some(clips);
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, shared: &Shared) -> Option<Nav> {
        let mut nav = None;
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading("Library");
            ui.add_space(8.0);
            let count = self.all_clips().count();
            if self.scanned && count > 0 {
                ui.label(
                    egui::RichText::new(format!(
                        "{} · {}",
                        format_bytes(library::total_bytes(&self.days)),
                        plural(count, "clip", "clips"),
                    ))
                    .strong(),
                )
                .on_hover_text(folder_list(&shared.config.library_folders()));
            }
            if self.scanning.is_some() {
                ui.spinner();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Settings").clicked() {
                    nav = Some(Nav::Settings);
                }
                if ui
                    .button("Refresh")
                    .on_hover_text("Read the folders again")
                    .clicked()
                {
                    self.media.retain(|_, m| !m.failed);
                    self.wanted.clear();
                    self.rescan(ui.ctx(), shared);
                }
                if !self.selected.is_empty() {
                    ui.separator();
                    if ui.button("Clear selection").clicked() {
                        self.selected.clear();
                    }
                    let delete =
                        egui::Button::new(egui::RichText::new("Delete…").color(Color32::WHITE))
                            .fill(Color32::from_rgb(190, 50, 50));
                    if ui.add(delete).clicked() {
                        self.ask_delete();
                    }
                    ui.label(format!("{} selected", self.selected.len()));
                }
            });
        });
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        ui.add_space(4.0);
        nav
    }

    fn body(&mut self, ui: &mut egui::Ui, shared: &Shared) -> Option<Nav> {
        if self.scanned && self.days.is_empty() {
            empty_state(ui, &shared.config.library_folders());
            return None;
        }

        let mut nav = None;
        let mut actions: Vec<(LibraryClip, TileAction)> = Vec::new();
        let mut visible_missing: Vec<Clip> = Vec::new();
        let mut other_missing: Vec<Clip> = Vec::new();
        let selecting = !self.selected.is_empty();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for day in &self.days {
                    day_header(ui, day);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = Vec2::splat(12.0);
                        for clip in &day.clips {
                            let media = self.media.get(&clip.clip.original);
                            let selected = self.selected.contains(&clip.clip.original);
                            let (rect, action) = tile(ui, clip, media, selected, selecting);
                            if let Some(action) = action {
                                actions.push((clip.clone(), action));
                            }
                            if media.is_none() {
                                if ui.is_rect_visible(rect) {
                                    visible_missing.push(clip.clip.clone());
                                } else {
                                    other_missing.push(clip.clip.clone());
                                }
                            }
                        }
                    });
                    ui.add_space(14.0);
                }
            });

        // Visible tiles first, then the rest, so scrolling finds them made.
        let wanted: Vec<Clip> = visible_missing.into_iter().chain(other_missing).collect();
        let paths: Vec<PathBuf> = wanted.iter().map(|c| c.original.clone()).collect();
        if paths != self.wanted {
            self.loader.want(wanted);
            self.wanted = paths;
        }

        for (clip, action) in actions {
            match action {
                TileAction::Open => nav = Some(Nav::Editor(clip.clip.original.clone())),
                TileAction::ToggleSelected => {
                    if !self.selected.remove(&clip.clip.original) {
                        self.selected.insert(clip.clip.original.clone());
                    }
                }
                TileAction::Star(starred) => {
                    match library::set_starred(&clip.clip, starred) {
                        Ok(_) => self.error = None,
                        Err(e) => self.error = Some(format!("Can't star the clip: {e}")),
                    }
                    self.rescan(ui.ctx(), shared);
                }
            }
        }
        nav
    }

    fn delete_dialog(&mut self, ctx: &egui::Context, shared: &Shared) {
        let Some(clips) = &self.confirm_delete else {
            return;
        };
        let mut close = false;
        let mut confirmed = false;
        let response = egui::Modal::new(egui::Id::unique("library-delete")).show(ctx, |ui| {
            ui.set_max_width(560.0);
            let bytes: u64 = clips.iter().map(|c| c.bytes).sum();
            ui.heading(format!(
                "Move {} to the trash?",
                plural(clips.len(), "clip", "clips")
            ));
            ui.add_space(4.0);
            ui.label(format!(
                "{} in all. Clip folders go with their edits and exports.",
                format_bytes(bytes)
            ));
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .max_height(260.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    egui::Grid::new("library-delete-list")
                        .num_columns(2)
                        .spacing([14.0, 4.0])
                        .show(ui, |ui| {
                            for clip in clips {
                                let target = library::deletion_target(&clip.clip);
                                let mut name = target
                                    .file_name()
                                    .unwrap_or(target.as_os_str())
                                    .to_string_lossy()
                                    .into_owned();
                                if clip.clip.in_folder {
                                    name.push('/');
                                }
                                ui.weak(format_bytes(clip.bytes));
                                ui.label(egui::RichText::new(name).monospace())
                                    .on_hover_text(target.display().to_string());
                                ui.end_row();
                            }
                        });
                });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let trash = egui::Button::new(
                    egui::RichText::new("Move to the trash").color(Color32::WHITE),
                )
                .fill(Color32::from_rgb(190, 50, 50));
                if ui.add(trash).clicked() {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if response.should_close() {
            close = true;
        }
        if confirmed {
            let targets: Vec<PathBuf> = clips
                .iter()
                .map(|c| library::deletion_target(&c.clip).to_path_buf())
                .collect();
            self.error = trash::delete_all(&targets)
                .err()
                .map(|e| format!("Can't move everything to the trash: {e}"));
            self.selected.clear();
            self.confirm_delete = None;
            self.rescan(ctx, shared);
        } else if close {
            self.confirm_delete = None;
        }
    }
}

fn day_header(ui: &mut egui::Ui, day: &Day) {
    let bytes: u64 = day.clips.iter().map(|c| c.bytes).sum();
    ui.horizontal(|ui| {
        let title = match weekday(&day.label) {
            Some(weekday) => format!("{weekday} {}", day.label),
            None => day.label.clone(),
        };
        ui.label(egui::RichText::new(title).size(17.0).strong());
        ui.weak(format!(
            "{} · {}",
            plural(day.clips.len(), "clip", "clips"),
            format_bytes(bytes)
        ));
    });
    ui.add_space(4.0);
}

/// Draws one tile. Returns its rectangle, and what was clicked.
fn tile(
    ui: &mut egui::Ui,
    clip: &LibraryClip,
    media: Option<&TileMedia>,
    selected: bool,
    selecting: bool,
) -> (Rect, Option<TileAction>) {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(TILE_WIDTH, TILE_HEIGHT), Sense::click());
    let name = clip.clip.name().to_string_lossy();
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &name));
    if !ui.is_rect_visible(rect) {
        return (rect, None);
    }
    let mut action = None;
    let visuals = ui.visuals().clone();
    let painter = ui.painter_at(rect.expand(2.0));
    let radius = CornerRadius::same(6);
    let hovered = response.hovered();

    // Card.
    let fill = if hovered {
        visuals.widgets.hovered.weak_bg_fill
    } else {
        visuals.faint_bg_color
    };
    painter.rect_filled(rect, radius, fill);

    // Thumbnail, cropped to 16:9.
    let thumb = Rect::from_min_size(rect.min, THUMB_SIZE);
    let thumb_radius = CornerRadius {
        nw: 6,
        ne: 6,
        sw: 0,
        se: 0,
    };
    painter.rect_filled(thumb, thumb_radius, Color32::from_gray(24));
    match media {
        Some(TileMedia {
            texture: Some(texture),
            ..
        }) => {
            let uv = cover_uv(texture.size_vec2(), THUMB_SIZE);
            egui::Image::new(texture)
                .uv(uv)
                .corner_radius(thumb_radius)
                .paint_at(ui, thumb);
        }
        Some(TileMedia { failed: true, .. }) => {
            painter.text(
                thumb.center(),
                Align2::CENTER_CENTER,
                "No preview",
                FontId::proportional(13.0),
                Color32::from_gray(140),
            );
        }
        _ => {
            painter.text(
                thumb.center(),
                Align2::CENTER_CENTER,
                "…",
                FontId::proportional(20.0),
                Color32::from_gray(110),
            );
        }
    }

    // Length, over the thumbnail's corner.
    if let Some(duration) = media.and_then(|m| m.duration) {
        let text = format_duration(duration);
        let galley = painter.layout_no_wrap(text, FontId::proportional(12.0), Color32::WHITE);
        let pill = Rect::from_min_size(
            thumb.right_bottom() - galley.size() - Vec2::new(12.0, 8.0),
            galley.size() + Vec2::new(8.0, 3.0),
        );
        painter.rect_filled(pill, CornerRadius::same(3), Color32::from_black_alpha(190));
        painter.galley(pill.min + Vec2::new(4.0, 1.5), galley, Color32::WHITE);
    }

    // Name, then size and badges.
    let text_left = rect.left() + 8.0;
    let name_rect = Rect::from_min_max(
        egui::pos2(text_left, thumb.bottom() + 6.0),
        egui::pos2(rect.right() - 8.0, thumb.bottom() + 24.0),
    );
    let name_galley = elided(ui, &name, FontId::proportional(14.0), name_rect.width());
    painter.galley(name_rect.min, name_galley, visuals.strong_text_color());

    let line_y = thumb.bottom() + 28.0;
    let size_galley = painter.layout_no_wrap(
        format_bytes(clip.bytes),
        FontId::proportional(12.5),
        visuals.weak_text_color(),
    );
    painter.galley(egui::pos2(text_left, line_y), size_galley, Color32::WHITE);
    let mut badge_right = rect.right() - 8.0;
    for (shown, label, color) in [
        (clip.has_export, "EXPORT", EXPORT_BADGE),
        (clip.has_edit, "EDIT", EDIT_BADGE),
    ] {
        if !shown {
            continue;
        }
        let galley =
            painter.layout_no_wrap(label.into(), FontId::proportional(10.5), Color32::WHITE);
        let size = galley.size() + Vec2::new(10.0, 4.0);
        let badge = Rect::from_min_size(egui::pos2(badge_right - size.x, line_y), size);
        painter.rect_filled(badge, CornerRadius::same(3), color);
        painter.galley(badge.min + Vec2::new(5.0, 2.0), galley, Color32::WHITE);
        badge_right = badge.left() - 5.0;
    }

    // Selection outline.
    if selected {
        painter.rect_stroke(
            rect,
            radius,
            Stroke::new(2.5, visuals.selection.stroke.color),
            egui::StrokeKind::Outside,
        );
    }

    // Star, over the thumbnail's top-right corner.
    let star_rect = Rect::from_min_size(
        egui::pos2(thumb.right() - 34.0, thumb.top() + 4.0),
        Vec2::splat(30.0),
    );
    let star = ui.interact(
        star_rect,
        ui.scope_id().with(("star", &clip.clip.original)),
        Sense::click(),
    );
    let star_name = if clip.starred { "Unstar" } else { "Star" };
    star.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, star_name));
    if clip.starred || hovered || star.hovered() {
        let backdrop = if star.hovered() { 170 } else { 110 };
        painter.circle_filled(
            star_rect.center(),
            14.0,
            Color32::from_black_alpha(backdrop),
        );
        paint_star(&painter, star_rect.center(), 10.0, clip.starred);
        let tip = if clip.starred {
            "Starred: kept. Click to unstar"
        } else {
            "Star to mark as kept"
        };
        if star.on_hover_text(tip).clicked() {
            action = Some(TileAction::Star(!clip.starred));
        }
    }

    // Checkbox, over the top-left corner. Painted, not a widget put
    // there: `Ui::put` would move the wrapping layout's cursor.
    if selected || selecting || hovered {
        let check_rect = Rect::from_min_size(thumb.min + Vec2::new(7.0, 7.0), Vec2::splat(20.0));
        let check = ui.interact(
            check_rect,
            ui.scope_id().with(("select", &clip.clip.original)),
            Sense::click(),
        );
        check.widget_info(|| {
            let mut info = egui::WidgetInfo::labeled(egui::Role::CheckBox, true, "Select");
            info.selected = Some(selected);
            info
        });
        let fill = if selected {
            visuals.selection.bg_fill
        } else if check.hovered() {
            Color32::from_black_alpha(200)
        } else {
            Color32::from_black_alpha(130)
        };
        painter.rect(
            check_rect,
            CornerRadius::same(4),
            fill,
            Stroke::new(1.5, Color32::from_white_alpha(200)),
            egui::StrokeKind::Inside,
        );
        if selected {
            let r = check_rect.shrink(4.5);
            painter.add(egui::Shape::line(
                vec![
                    egui::pos2(r.left(), r.center().y),
                    egui::pos2(r.center().x - 1.0, r.bottom()),
                    egui::pos2(r.right(), r.top()),
                ],
                Stroke::new(2.2, Color32::WHITE),
            ));
        }
        if check
            .on_hover_text("Select (or Ctrl/Cmd-click the clip)")
            .clicked()
        {
            action = Some(TileAction::ToggleSelected);
        }
    }

    if action.is_none() && response.clicked() {
        let command = ui.input(|i| i.modifiers.command);
        action = Some(if command {
            TileAction::ToggleSelected
        } else {
            TileAction::Open
        });
    }
    response.on_hover_text(clip.clip.original.display().to_string());
    (rect, action)
}

/// A five-pointed star of outer radius `r`: filled when starred, else an
/// outline. Painted rather than typed, so it shows whatever the fonts.
fn paint_star(painter: &egui::Painter, center: egui::Pos2, r: f32, filled: bool) {
    let inner = r * 0.45;
    let point = |i: usize| {
        let radius = if i.is_multiple_of(2) { r } else { inner };
        let angle = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
        center + radius * Vec2::angled(angle)
    };
    let outline: Vec<egui::Pos2> = (0..10).map(point).collect();
    if filled {
        // Not convex: the inner pentagon, then a triangle per point.
        let pentagon: Vec<egui::Pos2> = (0..5).map(|i| point(2 * i + 1)).collect();
        painter.add(egui::Shape::convex_polygon(pentagon, STAR, Stroke::NONE));
        for i in 0..5 {
            let tri = vec![point((2 * i + 9) % 10), point(2 * i), point(2 * i + 1)];
            painter.add(egui::Shape::convex_polygon(tri, STAR, Stroke::NONE));
        }
    } else {
        painter.add(egui::Shape::closed_line(
            outline,
            Stroke::new(1.6, Color32::from_white_alpha(230)),
        ));
    }
}

/// The part of a texture of `texture` size that covers `frame` without
/// stretching.
fn cover_uv(texture: Vec2, frame: Vec2) -> Rect {
    if texture.x <= 0.0 || texture.y <= 0.0 {
        return Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
    }
    let tex_aspect = texture.x / texture.y;
    let frame_aspect = frame.x / frame.y;
    if tex_aspect > frame_aspect {
        let w = frame_aspect / tex_aspect;
        Rect::from_min_max(
            egui::pos2((1.0 - w) / 2.0, 0.0),
            egui::pos2((1.0 + w) / 2.0, 1.0),
        )
    } else {
        let h = tex_aspect / frame_aspect;
        Rect::from_min_max(
            egui::pos2(0.0, (1.0 - h) / 2.0),
            egui::pos2(1.0, (1.0 + h) / 2.0),
        )
    }
}

/// `text` on one line, cut with an ellipsis to fit `width`.
fn elided(ui: &egui::Ui, text: &str, font: FontId, width: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, Color32::WHITE);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width);
    ui.painter().layout_job(job)
}

fn empty_state(ui: &mut egui::Ui, folders: &[PathBuf]) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.25);
        ui.label(egui::RichText::new("No clips yet").size(22.0).strong());
        ui.add_space(8.0);
        ui.label("Montage shows the videos in these folders:");
        ui.add_space(4.0);
        if folders.is_empty() {
            ui.weak("(none: add some in Settings → Library folders)");
        }
        for folder in folders {
            let missing = if folder.is_dir() {
                ""
            } else {
                "  (doesn't exist yet)"
            };
            ui.label(egui::RichText::new(format!("{}{missing}", folder.display())).monospace());
        }
        ui.add_space(8.0);
        ui.weak("Save a replay, or change the folders in Settings.");
    });
}

fn folder_list(folders: &[PathBuf]) -> String {
    let mut text = String::from("Total size of the clips in:");
    for f in folders {
        text.push_str(&format!("\n{}", f.display()));
    }
    text
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `648 MB`, `1.2 GB`, `85 KB`: decimal units, as file managers show.
pub(crate) fn format_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if b < 1e3 {
        format!("{bytes} B")
    } else if b < 1e6 {
        format!("{:.0} KB", b / 1e3)
    } else if b < 1e7 {
        format!("{:.1} MB", b / 1e6)
    } else if b < 1e9 {
        format!("{:.0} MB", b / 1e6)
    } else {
        format!("{:.1} GB", b / 1e9)
    }
}

/// `0:42`, `12:05`, `1:02:03`.
fn format_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The day of the week of a `YYYY-MM-DD` label.
fn weekday(label: &str) -> Option<&'static str> {
    let mut parts = label.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (parts.next()??, parts.next()??, parts.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from 1970-01-01 (Hinnant's days_from_civil), 1970-01-01 being a
    // Thursday.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    const NAMES: [&str; 7] = [
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
    ];
    Some(NAMES[days.rem_euclid(7) as usize])
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ui::FontsHandle;
    use montage_core::Config;
    use montage_core::render::RgbaImage;
    use std::fs;
    use std::path::Path;
    use std::time::UNIX_EPOCH;

    #[test]
    fn sizes_and_lengths_read_well() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(85_000), "85 KB");
        assert_eq!(format_bytes(4_200_000), "4.2 MB");
        assert_eq!(format_bytes(648_000_000), "648 MB");
        assert_eq!(format_bytes(1_234_000_000), "1.2 GB");
        assert_eq!(format_duration(42.4), "0:42");
        assert_eq!(format_duration(725.0), "12:05");
        assert_eq!(format_duration(3723.0), "1:02:03");
        assert_eq!(weekday("2026-11-07"), Some("Saturday"));
        assert_eq!(weekday("2000-02-29"), Some("Tuesday"));
        assert_eq!(weekday("1970-01-01"), Some("Thursday"));
        assert_eq!(weekday("Replays"), None);
    }

    #[test]
    fn thumbnails_cover_the_tile() {
        let wide = cover_uv(Vec2::new(320.0, 90.0), THUMB_SIZE);
        assert!(wide.min.x > 0.0 && wide.max.x < 1.0);
        assert_eq!((wide.min.y, wide.max.y), (0.0, 1.0));
        let exact = cover_uv(Vec2::new(320.0, 180.0), THUMB_SIZE);
        assert!((exact.width() - 1.0).abs() < 1e-4 && (exact.height() - 1.0).abs() < 1e-4);
    }

    fn write(path: &Path, bytes: usize, mtime: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![0u8; bytes]).unwrap();
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_secs(mtime))
            .unwrap();
    }

    /// A gradient whose hue depends on the clip's name, standing in for a
    /// decoded frame.
    fn fake_media(clip: &Clip) -> Media {
        let seed = (clip.name().len() as u8).wrapping_mul(37);
        let (w, h) = (320u32, 180u32);
        let mut image = RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                image.data[i] = seed.wrapping_add((x / 3) as u8);
                image.data[i + 1] = (y as u8).wrapping_mul(2) / 2 + 40;
                image.data[i + 2] = 255 - seed;
                image.data[i + 3] = 255;
            }
        }
        if clip.name().to_string_lossy().contains("broken") {
            return Media {
                error: Some("can't decode".into()),
                ..Media::default()
            };
        }
        Media {
            image: Some(image),
            duration: Some(30.0 + seed as f64 / 4.0),
            error: None,
        }
    }

    /// Where the tests leave their renders: `snapshots` in the build folder.
    pub(crate) fn snapshot_dir() -> PathBuf {
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
        let dir = target.join("snapshots");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct State {
        view: LibraryView,
        shared: Shared,
        nav: Option<Nav>,
    }

    fn harness_for(folders: Vec<PathBuf>) -> egui_kittest::Harness<'static, State> {
        let config = Config {
            library: Some(folders),
            ..Config::default()
        };
        let mut shared = Shared {
            config,
            fonts: FontsHandle::default(),
        };
        let ctx = egui::Context::default();
        let view = LibraryView::with_loader(&ctx, &mut shared, Arc::new(fake_media));
        let mut harness = egui_kittest::Harness::builder()
            .with_size(Vec2::new(1100.0, 720.0))
            .wgpu()
            .build_ui_state(
                |ui, state: &mut State| {
                    if let Some(nav) = state.view.ui(ui, &mut state.shared) {
                        state.nav = Some(nav);
                    }
                },
                State {
                    view,
                    shared,
                    nav: None,
                },
            );
        // Let the scan and the workers finish.
        let started = Instant::now();
        loop {
            harness.step();
            let s = harness.state();
            let done = s.view.scanned
                && s.view.scanning.is_none()
                && s.view
                    .all_clips()
                    .all(|c| s.view.media.contains_key(&c.clip.original));
            if done || started.elapsed() > Duration::from_secs(10) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        harness.run_steps(3);
        harness
    }

    #[test]
    fn library_renders_clips_by_day() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Replays");
        const NOV7: u64 = 1_794_052_800;
        for (day, name, mb, t, extras) in [
            (
                "2026-11-07",
                "Replay_2026-11-07_21-03-11",
                85,
                NOV7 + 3000,
                true,
            ),
            (
                "2026-11-07",
                "Replay_2026-11-07_20-11-42",
                62,
                NOV7 + 2000,
                false,
            ),
            (
                "2026-11-07",
                "Replay_2026-11-07_19-55-03",
                40,
                NOV7 + 1000,
                false,
            ),
            (
                "2026-11-06",
                "Replay_2026-11-06_23-14-09",
                120,
                NOV7 - 50_000,
                true,
            ),
        ] {
            let folder = root.join(day).join(name);
            write(&folder.join(format!("{name}.mp4")), mb * 100_000, t);
            if extras {
                write(&folder.join(format!("{name}-share.mp4")), 200_000, t);
                fs::write(
                    folder.join("montage.toml"),
                    "version = 1\nstarred = true\n[cut]\nstart = 1.0\nend = 4.0\n",
                )
                .unwrap();
            }
        }
        write(
            &root.join("2026-11-06").join("Replay_2026-11-06_broken.mkv"),
            3_000_000,
            NOV7 - 60_000,
        );

        let mut harness = harness_for(vec![root.clone()]);
        {
            let s = harness.state();
            assert_eq!(s.view.days.len(), 2);
            assert_eq!(s.view.days[0].label, "2026-11-07");
            assert_eq!(s.view.days[0].clips.len(), 3);
            assert_eq!(s.view.days[1].clips.len(), 2);
            assert!(
                s.view
                    .media
                    .values()
                    .filter(|m| m.texture.is_some())
                    .count()
                    >= 4
            );
        }
        harness
            .render()
            .unwrap()
            .save(snapshot_dir().join("library.png"))
            .unwrap();

        // Ctrl/Cmd-click selects; the header offers Delete.
        let first = harness.state().view.days[0].clips[0].clip.original.clone();
        harness.state_mut().view.selected.insert(first.clone());
        harness.state_mut().view.ask_delete();
        harness.run_steps(3);
        harness
            .render()
            .unwrap()
            .save(snapshot_dir().join("library-delete.png"))
            .unwrap();
        let confirm = harness.state().view.confirm_delete.as_ref().unwrap();
        assert_eq!(confirm.len(), 1);
        assert_eq!(
            library::deletion_target(&confirm[0].clip),
            first.parent().unwrap()
        );
    }

    #[test]
    fn clicks_open_select_and_star() {
        use egui_kittest::kittest::Queryable;
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("Replays").join("2026-11-07");
        write(&day.join("a").join("a.mp4"), 10, 1_794_052_800);
        write(&day.join("loose.mp4"), 10, 1_794_052_000);
        let mut harness = harness_for(vec![tmp.path().join("Replays")]);

        // Ctrl/Cmd-click selects, twice unselects.
        harness
            .get_by_label("a")
            .click_modifiers(egui::Modifiers::COMMAND);
        harness.run_steps(2);
        assert_eq!(harness.state().view.selected.len(), 1);
        assert!(harness.state().nav.is_none());
        harness
            .get_by_label("a")
            .click_modifiers(egui::Modifiers::COMMAND);
        harness.run_steps(2);
        assert!(harness.state().view.selected.is_empty());

        // A click opens the editor.
        harness.get_by_label("loose").click();
        harness.run_steps(2);
        match &harness.state().nav {
            Some(Nav::Editor(path)) => assert_eq!(path, &day.join("loose.mp4")),
            _ => panic!("the editor didn't open"),
        }

        // Starring a loose clip adopts it into its folder, and doesn't open
        // it.
        harness.state_mut().nav = None;
        let stars: Vec<_> = harness.get_all_by_label("Star").collect();
        assert_eq!(stars.len(), 2);
        stars[1].click();
        harness.run_steps(2);
        let started = Instant::now();
        while harness.state().view.scanning.is_some() && started.elapsed() < Duration::from_secs(5)
        {
            harness.step();
            std::thread::sleep(Duration::from_millis(10));
        }
        harness.run_steps(2);
        assert!(day.join("loose").join("loose.mp4").exists());
        let s = harness.state();
        let loose = s
            .view
            .all_clips()
            .find(|c| c.clip.name() == "loose")
            .unwrap();
        assert!(loose.starred && loose.clip.in_folder);
        assert!(s.nav.is_none());
    }

    #[test]
    fn empty_library_says_where_it_looks() {
        let tmp = tempfile::tempdir().unwrap();
        let mut harness = harness_for(vec![
            tmp.path().join("Replays"),
            tmp.path().join("Recordings"),
        ]);
        assert!(harness.state().view.scanned);
        assert!(harness.state().view.days.is_empty());
        harness
            .render()
            .unwrap()
            .save(snapshot_dir().join("library-empty.png"))
            .unwrap();
    }
}
