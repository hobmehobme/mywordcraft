//! WordCraft for Android.
//!
//! Runs the same [`wordcraft_ui_egui::WordApp`] as the desktop and web apps, on a `NativeActivity`
//! through eframe's wgpu backend. Build with `cargo apk build -p wordcraft-android --lib`.
//!
//! Done: the Start screen (File › Home) on launch, one window, the app's private folder for
//! recovery copies, Open and Save in shared storage through the built-in file browser (needs "All
//! files access", which the bottom strip asks for and reports), an on-screen keyboard that only
//! shows when asked for, an interface size that shrinks on short screens (a phone in landscape) so
//! dialogs fit, and an Arabic keyboard of the app's own (a `NativeActivity` gets no text from the
//! system keyboard's non-Latin layouts, so it cannot type Arabic). Mouse input and the wheel come
//! from a patched winit (`patches/winit-android-mouse.patch`, applied by the build workflow).
//! Not yet done: the Android file pickers (Storage Access Framework, which needs Java glue),
//! preferences loading and saving, the keyboard's inset, and touch-sized controls.
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
    use serde_json::json;
    use winit::platform::android::activity::AndroidApp;
    use wordcraft_engine::Session;
    use wordcraft_ui_egui::{Services, WordApp};

    use crate::storage;

    /// Room kept at the top for the status bar: winit has no safe area for Android yet
    /// (<https://github.com/rust-windowing/winit/issues/3910>).
    const STATUS_BAR_POINTS: f32 = 32.0;

    /// Shared storage of the main user: Documents, Download, and so on.
    const SHARED_STORAGE: &str = "/storage/emulated/0";

    /// The least screen height, in points, the interface is drawn for: a dialog such as Save As
    /// needs about this much with the status bar and the bottom strip. A shorter screen (a phone
    /// in landscape) gets a smaller interface size; a tablet keeps the system's own (Auto).
    const MIN_LOGICAL_HEIGHT: f32 = 600.0;

    /// The interface size, in percent, that gives a screen `px_height` pixels tall at least
    /// [`MIN_LOGICAL_HEIGHT`] points; `None` (Auto) when the system's own scale already does.
    fn fitted_percent(px_height: f32, native: f32) -> Option<u16> {
        if px_height / native.max(0.1) >= MIN_LOGICAL_HEIGHT {
            return None;
        }
        Some((px_height / MIN_LOGICAL_HEIGHT * 100.0).floor().clamp(50.0, 300.0) as u16)
    }

    /// How often the bottom strip looks at the storage permission again (after Settings).
    const STORAGE_CHECK_EVERY: Duration = Duration::from_secs(1);

    /// Name of the Arabic font added to the interface fonts for the keyboard's keys.
    const ARABIC_FONT_NAME: &str = "android arabic keys";

    /// Folders of Android's own fonts.
    const SYSTEM_FONT_DIRS: [&str; 3] = ["/system/fonts", "/system/product/fonts", "/product/fonts"];

    /// Fonts that draw Arabic, best first.
    const ARABIC_FONT_FILES: [&str; 4] = ["NotoNaskhArabic-Regular.ttf", "NotoNaskhArabicUI-Regular.ttf", "NotoSansArabic-Regular.ttf", "NotoSansArabicUI-Regular.ttf"];

    /// Largest font file read for the keys (a regular Arabic face is well under 1 MB).
    const ARABIC_FONT_MAX_BYTES: u64 = 8 << 20;

    const LETTER_ROWS: [&[&str]; 4] = [
        &["ض", "ص", "ث", "ق", "ف", "غ", "ع", "ه", "خ", "ح", "ج", "د"],
        &["ش", "س", "ي", "ب", "ل", "ا", "ت", "ن", "م", "ك", "ط"],
        &["ئ", "ء", "ؤ", "ر", "لا", "ى", "ة", "و", "ز", "ظ", "ذ"],
        &["أ", "إ", "آ", "،", "؛", "؟", "ـ"],
    ];

    /// Fatha, fathatan, damma, dammatan, kasra, kasratan / sukun, shadda, dagger alef, maddah,
    /// hamza above, hamza below.
    const MARK_ROWS: [&[&str]; 2] = [
        &["\u{064E}", "\u{064B}", "\u{064F}", "\u{064C}", "\u{0650}", "\u{064D}"],
        &["\u{0652}", "\u{0651}", "\u{0670}", "\u{0653}", "\u{0654}", "\u{0655}"],
    ];

    const DIGIT_ROWS: [&[&str]; 3] = [
        &["١", "٢", "٣", "٤", "٥", "٦", "٧", "٨", "٩", "٠"],
        &["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"],
        &[".", ":", "-", "(", ")", "/", "%", "!", "\"", "'"],
    ];

    /// Which keys the Arabic keyboard shows.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Layer {
        Letters,
        Marks,
        Digits,
    }

    /// The Arabic keyboard drawn by the app: tapped keys reach the document as ordinary text and
    /// key events, the way a hardware keyboard's do.
    struct ArabicKeys {
        open: bool,
        layer: Layer,
        /// An Arabic font read from the system, for the keys' labels.
        font: Option<Vec<u8>>,
        /// Keys tapped, handed to the app at the start of the next frame.
        pending: Vec<egui::Event>,
    }

    impl ArabicKeys {
        fn new() -> Self {
            let font = load_arabic_font();
            match &font {
                Some(bytes) => log::info!("Arabic keys font: {} bytes", bytes.len()),
                None => log::warn!("no Arabic font found in the system font folders: the Arabic keys will draw as boxes"),
            }
            Self { open: false, layer: Layer::Letters, font, pending: Vec::new() }
        }

        /// Adds the Arabic font to the interface fonts, again whenever the app has replaced them.
        fn ensure_font(&self, ctx: &egui::Context) {
            let Some(bytes) = &self.font else { return };
            if ctx.fonts(|f| f.definitions().font_data.contains_key(ARABIC_FONT_NAME)) {
                return;
            }
            ctx.add_font(egui::FontInsert {
                name: ARABIC_FONT_NAME.to_string(),
                data: egui::FontData::from_owned(bytes.clone()),
                families: vec![egui::InsertFontFamily { family: egui::FontFamily::Proportional, priority: egui::FontPriority::Lowest }],
            });
        }

        fn type_text(&mut self, ctx: &egui::Context, text: &str) {
            self.pending.push(egui::Event::Text(text.to_string()));
            ctx.request_repaint();
        }

        fn press(&mut self, ctx: &egui::Context, key: egui::Key) {
            for pressed in [true, false] {
                self.pending.push(egui::Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers: egui::Modifiers::NONE });
            }
            ctx.request_repaint();
        }

        fn show(&mut self, ui: &mut egui::Ui) {
            let rows: Vec<&[&str]> = match self.layer {
                Layer::Letters => LETTER_ROWS.to_vec(),
                Layer::Marks => MARK_ROWS.to_vec(),
                Layer::Digits => DIGIT_ROWS.to_vec(),
            };
            let spacing = 4.0;
            ui.spacing_mut().item_spacing = egui::vec2(spacing, spacing);
            let widest = rows.iter().map(|r| r.len()).max().unwrap_or(1).max(10) as f32;
            let key_w = ((ui.available_width() - spacing * (widest - 1.0)) / widest).clamp(26.0, 52.0);
            let key = egui::vec2(key_w, 42.0);
            let ctx = ui.ctx().clone();

            for row in rows {
                ui.horizontal(|ui| {
                    let row_w = row.len() as f32 * key_w + (row.len() as f32 - 1.0) * spacing;
                    ui.add_space(((ui.available_width() - row_w) / 2.0).max(0.0));
                    for &k in row {
                        // A mark is shown on a tatweel, so it has something to sit on.
                        let label = if self.layer == Layer::Marks { format!("\u{0640}{k}") } else { k.to_string() };
                        if ui.add(egui::Button::new(egui::RichText::new(label).size(22.0)).min_size(key)).clicked() {
                            self.type_text(&ctx, k);
                        }
                    }
                });
            }
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.layer, Layer::Letters, "ابج");
                ui.selectable_value(&mut self.layer, Layer::Marks, "\u{0640}\u{064E}");
                ui.selectable_value(&mut self.layer, Layer::Digits, "١٢٣");
                if ui.add(egui::Button::new("Space").min_size(egui::vec2(key_w * 4.0, 42.0))).clicked() {
                    self.type_text(&ctx, " ");
                }
                if ui.add(egui::Button::new("Back").min_size(egui::vec2(key_w * 1.5, 42.0))).clicked() {
                    self.press(&ctx, egui::Key::Backspace);
                }
                if ui.add(egui::Button::new("Enter").min_size(egui::vec2(key_w * 1.5, 42.0))).clicked() {
                    self.press(&ctx, egui::Key::Enter);
                }
            });
        }
    }

    /// The bytes of an Arabic font among Android's own, if any.
    fn load_arabic_font() -> Option<Vec<u8>> {
        let read = |path: &Path| std::fs::metadata(path).ok().filter(|m| m.len() <= ARABIC_FONT_MAX_BYTES).and_then(|_| std::fs::read(path).ok());
        for dir in SYSTEM_FONT_DIRS {
            for name in ARABIC_FONT_FILES {
                if let Some(bytes) = read(&Path::new(dir).join(name)) {
                    return Some(bytes);
                }
            }
        }
        for dir in SYSTEM_FONT_DIRS {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let regular = name.contains("Arabic") && !name.contains("Bold") && !name.contains("Italic") && (name.ends_with(".ttf") || name.ends_with(".otf"));
                if let Some(bytes) = regular.then(|| read(&entry.path())).flatten() {
                    return Some(bytes);
                }
            }
        }
        None
    }

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
        keys: ArabicKeys,
        storage: StorageStatus,
        /// The screen height, in pixels, the interface size was last fitted to.
        fitted_for: Option<u32>,
    }

    impl Shell {
        /// Sets the interface size for the screen's height whenever that height changes. The user can
        /// still pick another size in Options; it stays until the screen changes again.
        fn fit_interface(&mut self, ctx: &egui::Context) {
            let px = (ctx.content_rect().height() * ctx.pixels_per_point()).round() as u32;
            if self.fitted_for == Some(px) {
                return;
            }
            self.fitted_for = Some(px);
            let Some(native) = ctx.native_pixels_per_point() else { return };
            let percent = fitted_percent(px as f32, native).map_or(json!("auto"), |p| json!(p));
            match self.app.run("ui.interfaceSize", json!({ "percent": percent })) {
                Ok(_) => log::info!("interface size {percent} for a screen {px} px tall"),
                Err(e) => log::warn!("cannot set the interface size: {e}"),
            }
        }
    }

    impl eframe::App for Shell {
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            self.app.logic(ctx);
        }

        fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
            raw.events.append(&mut self.keys.pending);
            self.app.raw_input_hook(ctx, raw);
        }

        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            self.fit_interface(ui.ctx());
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
                    ui.toggle_value(&mut self.keys.open, "Arabic keyboard");
                    if self.storage.allowed == Some(false) && ui.button("Allow file access").clicked() {
                        storage::open_all_files_settings(&self.activity);
                    }
                    ui.weak(&self.storage.text);
                });
            });
            if self.keys.open {
                self.keys.ensure_font(ui.ctx());
                egui::Panel::bottom("android_arabic_keys").show(ui, |ui| self.keys.show(ui));
            }
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
                let mut session = Session::new(wordcraft_engine::blank_document(&region));
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
                // Like the desktop app: launch on File › Home (unless its Start screen option is off).
                let _ = app.start_screen();
                if let Some(dir) = wordcraft_control_key::settings_dir() {
                    let ctx = cc.egui_ctx.clone();
                    if let Err(e) = app.session.enable_recovery(dir.join("recovery"), Arc::new(move || ctx.request_repaint())) {
                        log::warn!("local recovery unavailable: {e}");
                    }
                }
                let storage = StorageStatus { allowed: None, text: String::new(), checked: None };
                Ok(Box::new(Shell { app, activity, keyboard: false, keys: ArabicKeys::new(), storage, fitted_for: None }))
            }),
        );
        if let Err(e) = result {
            log::error!("WordCraft stopped: {e}");
        }
    }
}
