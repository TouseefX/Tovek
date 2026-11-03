//! Tovek GUI — one egui/eframe app for Windows, Linux, macOS and Android.
//!
//! Four modes, mirroring every decompile surface the project ships:
//!   * **Single file** — the `luau-lifter <file>` path with every option.
//!   * **Folder** — compiles the *same* `batch.rs`/`decompile_core.rs` modules
//!     as the CLI binary, so output trees are byte-identical with
//!     `luau-lifter decompile-folder`. Validate-folder is included too.
//!   * **Server** — embeds the real `web-server` axum router
//!     (`/decompile`, `/decompile/raw`, `/decompile/batch`).
//!   * **Client scripts** — the executor-side Luau host scripts
//!     (`decompile.client.luau`, `decompile-batch.client.luau`), port-adjusted,
//!     with copy/save for each.
//!
//! On Android the app is built as a NativeActivity cdylib with xbuild (see
//! `manifest.yaml`); while the server runs it holds `FLAG_KEEP_SCREEN_ON` and a
//! partial wake lock so an on-device executor can keep streaming bytecode to it
//! as long as the app stays in the foreground.

// Same allocator as the CLI/server binaries: the decompiler is allocation-bound
// and mimalloc's per-thread free-lists measurably cut wall time. This crate is
// a leaf (app), so installing the global allocator here is safe.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ---------------------------------------------------------------------------
// The folder pipeline, compiled in source-for-source from the `luau-lifter`
// binary so the GUI's folder modes can never drift from the CLI:
//   * decompile_core.rs — decode/decompile/write path shared by both drivers
//   * decompile_cache.rs — the verified artifact cache (--cache-dir)
//   * batch.rs — `decompile-folder`
//   * validate.rs — `validate-folder` (needs an external `luau-analyze`)
// They are binary-side modules referring to the library by package name
// (`luau_lifter::…`), which is exactly what makes this inclusion possible.
// ---------------------------------------------------------------------------
#[allow(dead_code, unused_imports)]
#[path = "../../luau-lifter/src/decompile_core.rs"]
mod decompile_core;
#[allow(dead_code, unused_imports)]
#[path = "../../luau-lifter/src/decompile_cache.rs"]
mod decompile_cache;
#[allow(dead_code, unused_imports)]
#[path = "../../luau-lifter/src/batch.rs"]
mod batch;
#[allow(dead_code, unused_imports)]
#[path = "../../luau-lifter/src/validate.rs"]
mod validate;

mod app;
mod jobs;
mod picker;
mod scripts;

#[cfg(target_os = "android")]
mod android;

pub use app::TovekApp;

/// Process-wide decompiler setup: the quiet panic hook (per-function recovery
/// stays silent) and a rayon pool whose workers carry the decompile stack.
/// Call once, before any decompile work or server start.
pub fn init_decompiler() {
    luau_lifter::install_quiet_panic_hook();
    // 0 = one worker per logical CPU. The pool is created once per process;
    // the folder tab's "threads" box is forwarded but can only take effect on
    // the first configuration (same process-global rayon semantics as the CLI).
    let _ = luau_lifter::configure_thread_pool(0);
}

/// Android entry point. xbuild packages this crate's cdylib into the APK and
/// `NativeActivity` calls `android_main` through the `android-activity` glue.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(android_app: winit::platform::android::activity::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("tovek"),
    );
    log::info!("Tovek GUI starting (android_main)");

    init_decompiler();

    // Keep the screen on from the start: the whole point of the app on a phone
    // is to sit in the foreground while the executor streams scripts to it.
    // Best-effort — failure is logged, never fatal.
    if let Err(error) = android::keep_screen_on(true) {
        log::warn!("keep_screen_on failed: {error}");
    }

    // Writable without any permission prompt; used as the default output /
    // browse location. External app dir first (user-visible under
    // Android/data/dev.tovek.gui/files), internal as the fallback.
    let data_dir = android_app
        .external_data_path()
        .or_else(|| android_app.internal_data_path());

    let options = eframe::NativeOptions {
        android_app: Some(android_app.clone()),
        ..Default::default()
    };
    eframe::run_native(
        "Tovek",
        options,
        Box::new(move |cc| {
            Ok(Box::new(TovekApp::new(
                cc,
                data_dir.clone(),
                Some(android_app.clone()),
            )))
        }),
    )
    .expect("failed to start the Tovek GUI");
}
