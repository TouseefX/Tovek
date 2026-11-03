//! Desktop entry point (Windows, Linux, macOS). The Android entry point is
//! `android_main` in `lib.rs`, packaged by xbuild.

// Hide the console window on Windows release builds; everything the app has to
// say is shown in the UI.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> eframe::Result<()> {
    tovek_gui::init_decompiler();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Tovek V2.7")
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([720.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Tovek",
        options,
        Box::new(|cc| Ok(Box::new(tovek_gui::TovekApp::new(cc, None, None)))),
    )
}
