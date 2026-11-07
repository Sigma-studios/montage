//! The font picker: a searchable list of every installed family, Impact
//! pinned at the top, then the fonts picked lately, then all of them. Each
//! name is drawn in its own font, only for the rows on screen, and kept in a
//! bounded cache.

use std::collections::HashMap;
use std::hash::Hash;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, TextureHandle,
    TextureOptions, Vec2,
};
use montage_core::edit::{Color, DEFAULT_FONT};
use montage_core::render::RgbaImage;
use montage_text::Fonts;

use super::style;
use super::thumbs::to_color_image;

/// How many recent fonts the config keeps.
pub const MAX_RECENT: usize = 8;
/// How many names drawn in their font are kept.
const CACHE_SIZE: usize = 160;
/// How many names are drawn per frame, so scrolling stays smooth; the rest
/// come on the next frames.
const RENDERS_PER_FRAME: usize = 6;
const ROW_HEIGHT: f32 = 30.0;
const LABEL_PX: f32 = 19.0;

/// What the picker needs from the installed fonts.
pub trait FontSource {
    fn families(&self) -> &[String];
    fn has_family(&self, family: &str) -> bool;
    fn render_label(&mut self, family: &str, text: &str, px: f32, color: Color) -> RgbaImage;
}

impl FontSource for Fonts {
    fn families(&self) -> &[String] {
        Fonts::families(self)
    }
    fn has_family(&self, family: &str) -> bool {
        Fonts::has_family(self, family)
    }
    fn render_label(&mut self, family: &str, text: &str, px: f32, color: Color) -> RgbaImage {
        Fonts::render_label(self, family, text, px, color)
    }
}

/// `family` was just picked: it goes first in `recent`, which keeps at most
/// [`MAX_RECENT`] names. Impact is pinned anyway, so it isn't listed.
pub fn push_recent(recent: &mut Vec<String>, family: &str) {
    recent.retain(|f| f != family);
    if family != DEFAULT_FONT {
        recent.insert(0, family.to_string());
    }
    recent.truncate(MAX_RECENT);
}

/// A row of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Header(&'static str),
    Font(String),
}

/// The list for `query` (case-insensitive, anywhere in the name): Impact,
/// the recent fonts that are installed, then every family.
pub fn rows(
    families: &[String],
    recent: &[String],
    query: &str,
    installed: impl Fn(&str) -> bool,
) -> Vec<Row> {
    let query = query.trim().to_lowercase();
    let matches = |f: &str| query.is_empty() || f.to_lowercase().contains(&query);
    let mut rows = Vec::new();
    if matches(DEFAULT_FONT) {
        rows.push(Row::Font(DEFAULT_FONT.into()));
    }
    let recent: Vec<&String> = recent
        .iter()
        .filter(|f| f.as_str() != DEFAULT_FONT && installed(f) && matches(f))
        .collect();
    if !recent.is_empty() {
        rows.push(Row::Header("Recent"));
        rows.extend(recent.into_iter().map(|f| Row::Font(f.clone())));
    }
    let all: Vec<&String> = families.iter().filter(|f| matches(f)).collect();
    if !all.is_empty() {
        rows.push(Row::Header("All fonts"));
        rows.extend(all.into_iter().map(|f| Row::Font(f.clone())));
    }
    rows
}

/// A small least-recently-used cache.
pub struct Lru<K, V> {
    map: HashMap<K, (V, u64)>,
    tick: u64,
    capacity: usize,
}

impl<K: Hash + Eq + Clone, V> Lru<K, V> {
    pub fn new(capacity: usize) -> Lru<K, V> {
        Lru {
            map: HashMap::new(),
            tick: 0,
            capacity,
        }
    }

    pub fn get(&mut self, key: &K) -> Option<&V> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(key).map(|(v, used)| {
            *used = tick;
            &*v
        })
    }

    pub fn insert(&mut self, key: K, value: V) {
        self.tick += 1;
        self.map.insert(key, (value, self.tick));
        while self.map.len() > self.capacity {
            let oldest = self
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => self.map.remove(&k),
                None => break,
            };
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

pub struct FontPicker {
    query: String,
    labels: Lru<String, Option<TextureHandle>>,
    rows: Option<(String, Vec<String>, Vec<Row>)>,
}

impl Default for FontPicker {
    fn default() -> Self {
        FontPicker {
            query: String::new(),
            labels: Lru::new(CACHE_SIZE),
            rows: None,
        }
    }
}

impl FontPicker {
    /// The list, with its search field. Returns the family picked.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        fonts: &mut dyn FontSource,
        recent: &[String],
        current: &str,
        just_opened: bool,
    ) -> Option<String> {
        let mut picked = None;
        let search = ui
            .add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("Search fonts")
                    .desired_width(f32::INFINITY),
            )
            .accessible_name("Search fonts");
        if just_opened {
            search.request_focus();
        }

        let stale = self
            .rows
            .as_ref()
            .is_none_or(|(q, r, _)| *q != self.query || r != recent);
        if stale {
            let rows = rows(fonts.families(), recent, &self.query, |f| {
                fonts.has_family(f)
            });
            self.rows = Some((self.query.clone(), recent.to_vec(), rows));
        }
        let rows = &self.rows.as_ref().expect("just made").2;
        if rows.is_empty() {
            ui.label(RichText::new("No font matches.").color(style::SUBTEXT));
            return None;
        }
        if search.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter))
            && let Some(Row::Font(first)) = rows.iter().find(|r| matches!(r, Row::Font(_)))
        {
            return Some(first.clone());
        }

        let mut renders = 0;
        let mut waiting = false;
        let ppp = ui.pixels_per_point();
        egui::ScrollArea::vertical()
            .max_height(360.0)
            .auto_shrink([false, true])
            .show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
                for row in &rows[range] {
                    let (rect, response) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), ROW_HEIGHT),
                        match row {
                            Row::Header(_) => Sense::hover(),
                            Row::Font(_) => Sense::click(),
                        },
                    );
                    let painter = ui.painter_at(rect);
                    match row {
                        Row::Header(title) => {
                            painter.text(
                                Pos2::new(rect.left() + 6.0, rect.bottom() - 6.0),
                                Align2::LEFT_BOTTOM,
                                *title,
                                FontId::proportional(11.5),
                                style::SUBTEXT,
                            );
                        }
                        Row::Font(family) => {
                            if family == current {
                                painter.rect_filled(rect, CornerRadius::same(5), style::SELECTION);
                            } else if response.hovered() {
                                painter.rect_filled(
                                    rect,
                                    CornerRadius::same(5),
                                    Color32::from_white_alpha(14),
                                );
                            }
                            let texture = match self.labels.get(family) {
                                Some(t) => t.clone(),
                                None if renders < RENDERS_PER_FRAME => {
                                    renders += 1;
                                    let t = render_label(ui.ctx(), fonts, family, ppp);
                                    self.labels.insert(family.clone(), t.clone());
                                    t
                                }
                                None => {
                                    waiting = true;
                                    None
                                }
                            };
                            draw_label(&painter, rect, family, texture.as_ref(), ppp);
                            if response.clicked() {
                                picked = Some(family.clone());
                            }
                        }
                    }
                }
            });
        if waiting {
            ui.ctx().request_repaint();
        }
        picked
    }
}

fn render_label(
    ctx: &egui::Context,
    fonts: &mut dyn FontSource,
    family: &str,
    ppp: f32,
) -> Option<TextureHandle> {
    let image = fonts.render_label(family, family, LABEL_PX * ppp, Color::WHITE);
    (image.width > 0 && image.height > 0).then(|| {
        ctx.load_texture(
            format!("montage-font-{family}"),
            to_color_image(&image),
            TextureOptions::LINEAR,
        )
    })
}

/// The name in its own font, scaled down to fit the row; in the UI's font
/// while it's being drawn.
fn draw_label(
    painter: &egui::Painter,
    rect: Rect,
    family: &str,
    texture: Option<&TextureHandle>,
    ppp: f32,
) {
    let inner = rect.shrink2(Vec2::new(8.0, 3.0));
    let Some(texture) = texture else {
        painter.text(
            Pos2::new(inner.left(), inner.center().y),
            Align2::LEFT_CENTER,
            family,
            FontId::proportional(14.0),
            style::SUBTEXT,
        );
        return;
    };
    let [w, h] = texture.size();
    let mut size = Vec2::new(w as f32, h as f32) / ppp;
    if size.y > inner.height() {
        size *= inner.height() / size.y;
    }
    // Wide names are cut at the row's end rather than squeezed.
    let shown = size.x.min(inner.width());
    let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(shown / size.x.max(1.0), 1.0));
    let dest = Rect::from_min_size(
        Pos2::new(inner.left(), inner.center().y - size.y / 2.0),
        Vec2::new(shown, size.y),
    );
    painter.image(texture.id(), dest, uv, style::TEXT);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn recent_fonts_go_first_without_duplicates() {
        let mut recent = names(&["Arial", "Comic Sans MS"]);
        push_recent(&mut recent, "Inter");
        assert_eq!(recent, names(&["Inter", "Arial", "Comic Sans MS"]));
        push_recent(&mut recent, "Comic Sans MS");
        assert_eq!(recent, names(&["Comic Sans MS", "Inter", "Arial"]));
        // Impact is pinned, so it's never listed.
        push_recent(&mut recent, DEFAULT_FONT);
        assert_eq!(recent, names(&["Comic Sans MS", "Inter", "Arial"]));
        for i in 0..20 {
            push_recent(&mut recent, &format!("Font {i}"));
        }
        assert_eq!(recent.len(), MAX_RECENT);
        assert_eq!(recent[0], "Font 19");
        assert_eq!(recent[MAX_RECENT - 1], "Font 12");
    }

    #[test]
    fn impact_is_pinned_then_recent_then_all() {
        let families = names(&["Arial", "Fira Sans", "Impact", "Inter"]);
        let recent = names(&["Inter", "Gone Font"]);
        let installed = |f: &str| families.iter().any(|g| g == f);
        let list = rows(&families, &recent, "", installed);
        assert_eq!(
            list,
            vec![
                Row::Font("Impact".into()),
                Row::Header("Recent"),
                Row::Font("Inter".into()),
                Row::Header("All fonts"),
                Row::Font("Arial".into()),
                Row::Font("Fira Sans".into()),
                Row::Font("Impact".into()),
                Row::Font("Inter".into()),
            ]
        );
        let list = rows(&families, &recent, " FIRA", installed);
        assert_eq!(
            list,
            vec![Row::Header("All fonts"), Row::Font("Fira Sans".into())]
        );
        assert!(rows(&families, &recent, "zzz", installed).is_empty());
    }

    #[test]
    fn lru_evicts_the_least_recently_used() {
        let mut lru = Lru::new(2);
        lru.insert("a", 1);
        lru.insert("b", 2);
        assert_eq!(lru.get(&"a"), Some(&1));
        lru.insert("c", 3);
        assert_eq!(lru.len(), 2);
        assert_eq!(lru.get(&"b"), None, "b was the least recently used");
        assert_eq!(lru.get(&"a"), Some(&1));
        assert_eq!(lru.get(&"c"), Some(&3));
    }

    /// Draws each name as a bar whose shade depends on the family, so the
    /// snapshot shows which rows got their label.
    struct FakeFonts {
        families: Vec<String>,
        rendered: usize,
    }

    impl FontSource for FakeFonts {
        fn families(&self) -> &[String] {
            &self.families
        }
        fn has_family(&self, family: &str) -> bool {
            self.families.iter().any(|f| f == family)
        }
        fn render_label(&mut self, family: &str, text: &str, px: f32, _: Color) -> RgbaImage {
            self.rendered += 1;
            let (w, h) = ((text.len() as f32 * px * 0.55) as u32, px as u32);
            let mut image = RgbaImage::new(w, h);
            let shade = 120 + (family.len() as u32 * 13 % 120) as u8;
            for y in h / 4..h * 3 / 4 {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    image.data[i..i + 4].copy_from_slice(&[shade, shade, 255, 255]);
                }
            }
            image
        }
    }

    #[test]
    fn snapshot_of_the_picker() {
        let mut families: Vec<String> = (0..2900).map(|i| format!("Family {i:04}")).collect();
        families.extend(names(&["Impact", "Inter", "Comic Sans MS"]));
        families.sort_by_key(|f| f.to_lowercase());
        let state = (
            FontPicker::default(),
            FakeFonts {
                families,
                rendered: 0,
            },
        );
        let recent = names(&["Inter", "Comic Sans MS"]);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(320.0, 440.0))
            .build_ui_state(
                move |ui, (picker, fonts): &mut (FontPicker, FakeFonts)| {
                    super::super::style::apply(ui.style_mut());
                    picker.show(ui, fonts, &recent, "Inter", false);
                },
                state,
            );
        harness.run_steps(8);
        // Only the rows on screen are drawn, a few per frame.
        let rendered = harness.state().1.rendered;
        assert!((6..=20).contains(&rendered), "{rendered} labels drawn");

        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/agent-editor/snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        harness
            .render()
            .unwrap()
            .save(dir.join("font-picker.png"))
            .unwrap();
    }
}
