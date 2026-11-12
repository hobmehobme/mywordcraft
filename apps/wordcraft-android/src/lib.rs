//! WordCraft for Android.
//!
//! Runs the same [`wordcraft_ui_egui::WordApp`] as the desktop and web apps, on a `NativeActivity`
//! through eframe's wgpu backend. Build with `cargo apk build -p wordcraft-android --lib`.
//!
//! First milestone: the sample document, one window, the app's private folder for preferences and
//! recovery copies. Not yet done: Android file pickers (Storage Access Framework), preferences
//! loading and saving, user fonts, the on-screen keyboard's inset, and touch-sized controls.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

/// The entry point `NativeActivity` calls. The only unsafe code in this crate is the
/// `no_mangle` attribute, which the platform requires.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: winit::platform::android::activity::AndroidApp) {
    host::run(app);
}

#[cfg(target_os = "android")]
mod host {
    use std::sync::Arc;

    use eframe::egui;
    use winit::platform::android::activity::AndroidApp;
    use wordcraft_engine::Session;
    use wordcraft_ui_egui::{Services, WordApp};

    /// Room kept at the top for the status bar: winit has no safe area for Android yet
    /// (<https://github.com/rust-windowing/winit/issues/3910>).
    const STATUS_BAR_POINTS: f32 = 32.0;

    /// The app, as eframe drives it.
    struct Shell(WordApp);

    impl eframe::App for Shell {
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            self.0.logic(ctx);
        }

        fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
            self.0.raw_input_hook(ctx, raw);
        }

        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            egui::Panel::top("android_status_bar_space").show(ui, |ui| {
                ui.set_height(STATUS_BAR_POINTS);
            });
            self.0.ui(ui);
        }

        fn on_exit(&mut self) {
            self.0.session.finish_recovery();
        }
    }

    /// Starts the app on the activity `app` and returns when it closes.
    pub fn run(app: AndroidApp) {
        android_logger::init_once(android_logger::Config::default().with_tag("WordCraft").with_max_level(log::LevelFilter::Info));
        log::info!("WordCraft {} starting", env!("CARGO_PKG_VERSION"));

        // Android has no home directory: settings and recovery copies go in the app's private folder.
        match app.internal_data_path() {
            Some(dir) => {
                let settings = dir.join("settings");
                if let Err(e) = std::fs::create_dir_all(&settings) {
                    log::warn!("cannot create {}: {e}", settings.display());
                }
                wordcraft_control_key::set_settings_dir(settings);
            }
            None => log::warn!("no private data folder: recovery copies are off"),
        }

        let options = eframe::NativeOptions { android_app: Some(app), ..Default::default() };
        let result = eframe::run_native(
            "WordCraft",
            options,
            Box::new(|cc| {
                let region = wordcraft_ui_egui::i18n::system_region();
                let mut session = Session::new(wordcraft_engine::sample::sample_document());
                session.system_region = region;
                let mut app = WordApp::new(session, Services::default());
                // Updates arrive through the app store, not from inside the app.
                app.disable_self_update();
                if let Some(dir) = wordcraft_control_key::settings_dir() {
                    let ctx = cc.egui_ctx.clone();
                    if let Err(e) = app.session.enable_recovery(dir.join("recovery"), Arc::new(move || ctx.request_repaint())) {
                        log::warn!("local recovery unavailable: {e}");
                    }
                }
                Ok(Box::new(Shell(app)))
            }),
        );
        if let Err(e) = result {
            log::error!("WordCraft stopped: {e}");
        }
    }
}
