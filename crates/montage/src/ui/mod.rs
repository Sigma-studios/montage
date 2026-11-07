//! Montage's window: the library, the editor and the settings page, on
//! eframe (winit with egui-wgpu), so the same window runs on Wayland,
//! Windows and macOS.

pub mod editor;
pub mod library;
pub mod settings;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use eframe::egui;
use montage_core::Config;
use montage_text::Fonts;

/// What the window opens on.
pub enum Start {
    Library,
    Editor(PathBuf),
    Settings,
}

/// Where a view asks to go next.
pub enum Nav {
    Library,
    Editor(PathBuf),
    Settings,
}

pub fn run(start: Start) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Montage")
            .with_app_id("montage")
            .with_inner_size([1280.0, 860.0])
            .with_min_inner_size([820.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Montage",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, start)))),
    )
    .map_err(|e| anyhow!("can't open the window: {e}"))
}

/// What every view shares.
pub struct Shared {
    pub config: Config,
    pub fonts: FontsHandle,
}

impl Shared {
    /// Writes the config back (recent fonts, settings).
    pub fn save_config(&self) -> Result<()> {
        let path = Config::path().ok_or_else(|| anyhow!("no config folder on this system"))?;
        self.config.save_to(&path)?;
        Ok(())
    }
}

/// The installed fonts, loaded on a thread when the window opens.
#[derive(Clone, Default)]
pub struct FontsHandle {
    loaded: Arc<Mutex<Option<Arc<Mutex<Fonts>>>>>,
}

impl FontsHandle {
    fn start_loading(ctx: &egui::Context) -> FontsHandle {
        let handle = FontsHandle::default();
        let slot = handle.loaded.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let fonts = Fonts::load();
            *slot.lock().unwrap() = Some(Arc::new(Mutex::new(fonts)));
            ctx.request_repaint();
        });
        handle
    }

    /// The fonts, once loaded.
    pub fn get(&self) -> Option<Arc<Mutex<Fonts>>> {
        self.loaded.lock().unwrap().clone()
    }
}

enum Screen {
    Library(library::LibraryView),
    Editor(editor::EditorView),
    Settings(settings::SettingsView),
}

struct App {
    shared: Shared,
    screen: Screen,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, start: Start) -> App {
        let config = Config::load().unwrap_or_else(|e| {
            eprintln!("montage: {e}; using the default settings");
            Config::default()
        });
        let mut shared = Shared {
            config,
            fonts: FontsHandle::start_loading(&cc.egui_ctx),
        };
        let nav = match start {
            Start::Library => Nav::Library,
            Start::Editor(path) => Nav::Editor(path),
            Start::Settings => Nav::Settings,
        };
        let screen = open(nav, &cc.egui_ctx, &mut shared);
        App { shared, screen }
    }
}

fn open(nav: Nav, ctx: &egui::Context, shared: &mut Shared) -> Screen {
    match nav {
        Nav::Library => Screen::Library(library::LibraryView::new(ctx, shared)),
        Nav::Editor(path) => Screen::Editor(editor::EditorView::new(ctx, shared, path)),
        Nav::Settings => Screen::Settings(settings::SettingsView::new(ctx, shared)),
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let nav = match &mut self.screen {
            Screen::Library(view) => view.ui(ui, &mut self.shared),
            Screen::Editor(view) => view.ui(ui, &mut self.shared),
            Screen::Settings(view) => view.ui(ui, &mut self.shared),
        };
        if let Some(nav) = nav {
            self.screen = open(nav, ui.ctx(), &mut self.shared);
        }
    }
}
