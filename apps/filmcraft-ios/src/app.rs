//! The app eframe runs on iPadOS: FilmCraft's UI plus what the host owns (safe area, taking saved
//! projects and exports to where the user keeps them).
use eframe::egui;
use filmcraft_ui_egui::FilmcraftApp;

pub struct IosApp {
    app: FilmcraftApp,
    last_safe_area: Option<String>,
    /// Takes the files the app saves to where the user keeps them (none: no app container).
    sync: Option<crate::documents::DocumentSync>,
}

impl IosApp {
    pub fn new(app: FilmcraftApp, root: Option<std::path::PathBuf>) -> Self {
        Self { app, last_safe_area: None, sync: root.map(crate::documents::DocumentSync::new) }
    }
}

impl eframe::App for IosApp {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        crate::safe_area::report(ctx, &mut self.last_safe_area);
        eframe::App::logic(&mut self.app, ctx, frame);
        if let Some(sync) = &mut self.sync {
            sync.poll(|message| self.app.ui.status = message);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        eframe::App::ui(&mut self.app, ui, frame);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        eframe::App::raw_input_hook(&mut self.app, ctx, raw_input);
        crate::safe_area::apply(raw_input);
    }

    fn on_exit(&mut self) {
        eframe::App::on_exit(&mut self.app);
    }
}
