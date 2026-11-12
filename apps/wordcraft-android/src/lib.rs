//! WordCraft for Android.
//!
//! Runs the same [`wordcraft_ui_egui::WordApp`] as the desktop and web apps, on a `NativeActivity`
//! through eframe's wgpu backend. Build with `cargo apk build -p wordcraft-android --lib`.
//!
//! Done: the sample document, one window, the app's private folder for recovery copies, Open and
//! Save in shared storage through the built-in file browser (needs "All files access", which the
//! bottom strip asks for and reports), and an on-screen keyboard that only shows when asked for.
//! Not yet done: the Android file pickers (Storage Access Framework, which needs Java glue),
//! preferences loading and saving, user fonts, the keyboard's inset, and touch-sized controls.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

/// The entry point `NativeActivity` calls. Unsafe code in this crate: this `no_mangle`
/// attribute (the platform requires it), one `set_var` in `host::run`, and the two JNI handles in
/// `storage::with_activity`.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: winit::platform::android::activity::AndroidApp) {
    host::run(app);
}

/// Android's "All files access" (`MANAGE_EXTERNAL_STORAGE`), through JNI.
#[cfg(target_os = "android")]
mod storage {
    use jni::objects::{JObject, JString, JValue};
    use jni::{JNIEnv, JavaVM};
    use winit::platform::android::activity::AndroidApp;

    /// Runs `f` on this thread with a JNI environment and the activity. `None` when it fails (a
    /// Java exception is cleared and logged).
    fn with_activity<R>(app: &AndroidApp, f: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<R>) -> Option<R> {
        // SAFETY: android-activity documents both pointers as valid for as long as the activity
        // lives, and this runs on the app's own thread while it does.
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }.ok()?;
        // SAFETY: as above; `JObject` does not own the reference, so dropping it frees nothing.
        let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
        let mut env = vm.attach_current_thread().ok()?;
        match f(&mut env, &activity) {
            Ok(value) => Some(value),
            Err(e) => {
                let _ = env.exception_clear();
                log::warn!("JNI call failed: {e}");
                None
            }
        }
    }

    /// Whether the app may use all files (`None`: Android 10 or older, or the question failed).
    pub fn all_files_allowed(app: &AndroidApp) -> Option<bool> {
        with_activity(app, |env, _| env.call_static_method("android/os/Environment", "isExternalStorageManager", "()Z", &[])?.z())
    }

    /// Opens Settings on this app's "All files access" switch; on failure, on the list of apps.
    pub fn open_all_files_settings(app: &AndroidApp) {
        let for_this_app = with_activity(app, |env, activity| {
            let name: JString = env.call_method(activity, "getPackageName", "()Ljava/lang/String;", &[])?.l()?.into();
            let package: String = env.get_string(&name)?.into();
            let action = env.new_string("android.settings.MANAGE_APP_ALL_FILES_ACCESS_PERMISSION")?;
            let intent = env.new_object("android/content/Intent", "(Ljava/lang/String;)V", &[JValue::Object(&action)])?;
            let text = env.new_string(format!("package:{package}"))?;
            let uri = env.call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[JValue::Object(&text)])?.l()?;
            env.call_method(&intent, "setData", "(Landroid/net/Uri;)Landroid/content/Intent;", &[JValue::Object(&uri)])?;
            env.call_method(activity, "startActivity", "(Landroid/content/Intent;)V", &[JValue::Object(&intent)])?;
            Ok(())
        });
        if for_this_app.is_some() {
            return;
        }
        let _ = with_activity(app, |env, activity| {
            let action = env.new_string("android.settings.MANAGE_ALL_FILES_ACCESS_PERMISSION")?;
            let intent = env.new_object("android/content/Intent", "(Ljava/lang/String;)V", &[JValue::Object(&action)])?;
            env.call_method(activity, "startActivity", "(Landroid/content/Intent;)V", &[JValue::Object(&intent)])?;
            Ok(())
        });
    }
}

#[cfg(target_os = "android")]
mod host {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use eframe::egui;
    use winit::platform::android::activity::AndroidApp;
    use wordcraft_engine::Session;
    use wordcraft_ui_egui::{Services, WordApp};

    use crate::storage;

    /// Room kept at the top for the status bar: winit has no safe area for Android yet
    /// (<https://github.com/rust-windowing/winit/issues/3910>).
    const STATUS_BAR_POINTS: f32 = 32.0;

    /// Shared storage of the main user: Documents, Download, and so on.
    const SHARED_STORAGE: &str = "/storage/emulated/0";

    /// How often the bottom strip looks at the storage permission again (after Settings).
    const STORAGE_CHECK_EVERY: Duration = Duration::from_secs(1);

    /// What the bottom strip says about shared storage.
    struct StorageStatus {
        /// Whether "All files access" is on; `None` when Android does not say.
        allowed: Option<bool>,
        /// What listing the shared storage folder gave, in a few words.
        text: String,
        checked: Option<Instant>,
    }

    impl StorageStatus {
        fn refresh(&mut self, activity: &AndroidApp) {
            self.checked = Some(Instant::now());
            let allowed = storage::all_files_allowed(activity);
            let text = match std::fs::read_dir(SHARED_STORAGE) {
                Ok(entries) => format!("Storage: {} items visible", entries.take(1000).count()),
                Err(e) => format!("Storage: {e}"),
            };
            if allowed != self.allowed || text != self.text {
                log::info!("all files access: {allowed:?}; {text}");
            }
            self.allowed = allowed;
            self.text = text;
        }

        fn due(&self) -> bool {
            self.checked.is_none_or(|t| t.elapsed() >= STORAGE_CHECK_EVERY)
        }
    }

    /// The app, as eframe drives it.
    struct Shell {
        app: WordApp,
        activity: AndroidApp,
        /// Whether the on-screen keyboard may show. Off at start: a document that always asks for
        /// the keyboard would bring it up on every tap and cover the page.
        keyboard: bool,
        storage: StorageStatus,
    }

    impl eframe::App for Shell {
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            self.app.logic(ctx);
        }

        fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
            self.app.raw_input_hook(ctx, raw);
        }

        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            if self.storage.due() {
                self.storage.refresh(&self.activity);
            }
            ui.ctx().request_repaint_after(STORAGE_CHECK_EVERY);

            egui::Panel::top("android_status_bar_space").show(ui, |ui| {
                ui.set_height(STATUS_BAR_POINTS);
            });
            egui::Panel::bottom("android_strip").show(ui, |ui| {
                ui.spacing_mut().button_padding = egui::vec2(14.0, 8.0);
                ui.horizontal_wrapped(|ui| {
                    ui.toggle_value(&mut self.keyboard, "On-screen keyboard");
                    if self.storage.allowed == Some(false) && ui.button("Allow file access").clicked() {
                        storage::open_all_files_settings(&self.activity);
                    }
                    ui.weak(&self.storage.text);
                });
            });
            self.app.ui(ui);
            // Without an input method request the system keeps the keyboard hidden. A hardware
            // keyboard still types: its keys arrive as ordinary key and text events.
            if !self.keyboard {
                ui.ctx().output_mut(|o| o.ime = None);
            }
        }

        fn on_exit(&mut self) {
            self.app.session.finish_recovery();
        }
    }

    /// Starts the app on the activity `app` and returns when it closes.
    pub fn run(app: AndroidApp) {
        android_logger::init_once(android_logger::Config::default().with_tag("WordCraft").with_max_level(log::LevelFilter::Info));
        log::info!("WordCraft {} starting", env!("CARGO_PKG_VERSION"));

        // Settings and recovery copies go in the app's private folder.
        if let Some(dir) = app.internal_data_path() {
            let settings = dir.join("settings");
            if let Err(e) = std::fs::create_dir_all(&settings) {
                log::warn!("cannot create {}: {e}", settings.display());
            }
            wordcraft_control_key::set_settings_dir(settings);
        } else {
            log::warn!("no private data folder: recovery copies are off");
        }

        // The built-in file browser starts in $HOME, which Android does not set: use shared storage.
        let shared = Path::new(SHARED_STORAGE);
        let home: PathBuf = if shared.is_dir() { shared.to_path_buf() } else { app.internal_data_path().unwrap_or_else(|| PathBuf::from("/")) };
        // SAFETY: this is the first thing the app thread does, before eframe starts any thread of
        // this crate's own, so nothing here reads the environment at the same time.
        unsafe { std::env::set_var("HOME", &home) };

        let activity = app.clone();
        let options = eframe::NativeOptions { android_app: Some(app), ..Default::default() };
        let result = eframe::run_native(
            "WordCraft",
            options,
            Box::new(move |cc| {
                let region = wordcraft_ui_egui::i18n::system_region();
                let mut session = Session::new(wordcraft_engine::sample::sample_document());
                session.system_region = region;
                let services = Services {
                    // Android's own pickers need Java glue; an `Err` makes the app show its
                    // built-in file browser for Open, Save As, Export and Insert Picture.
                    file_dialog: Some(Box::new(|_request| {
                        let (tx, rx) = std::sync::mpsc::channel();
                        let _ = tx.send(Err("no system file dialog on Android yet".to_string()));
                        rx
                    })),
                    ..Services::default()
                };
                let mut app = WordApp::new(session, services);
                // Updates arrive through the app store, not from inside the app.
                app.disable_self_update();
                if let Some(dir) = wordcraft_control_key::settings_dir() {
                    let ctx = cc.egui_ctx.clone();
                    if let Err(e) = app.session.enable_recovery(dir.join("recovery"), Arc::new(move || ctx.request_repaint())) {
                        log::warn!("local recovery unavailable: {e}");
                    }
                }
                let storage = StorageStatus { allowed: None, text: String::new(), checked: None };
                Ok(Box::new(Shell { app, activity, keyboard: false, storage }))
            }),
        );
        if let Err(e) = result {
            log::error!("WordCraft stopped: {e}");
        }
    }
}
