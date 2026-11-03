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
//! `manifest.yaml`). The embedded server does not run in the UI process
//! there: `TovekService` (a foreground service in its own `:tovek_server`
//! process, injected into the APK by CI) loads this same library and drives
//! the server through the `nativeStartServer`/`nativeStopServer` exports
//! below — so the server keeps listening even after the app is swiped away,
//! Termux-style, until Stop is pressed in the app or on the notification.

// Same allocator as the CLI/server binaries: the decompiler is allocation-bound
// and mimalloc's per-thread free-lists measurably cut wall time. This crate is
// a leaf (app), so installing the global allocator here is safe.
//
// NOT on Android: libmimalloc-sys builds its C code with
// `-ftls-model=initial-exec`, and Bionic's dynamic linker rejects IE-model TLS
// in a dlopen()ed library — NativeActivity could not load libtovek_gui.so
// ("TLS symbol \"(null)\" … using IE access model"). Android uses the system
// allocator (scudo) instead.
#[cfg(not(target_os = "android"))]
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
    // Goes through ANativeActivity_setWindowFlags (thread-safe, infallible).
    android::keep_screen_on(&android_app, true);

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

/// JNI entry points for `TovekService` (tovek-gui/android/TovekService.java).
///
/// The service runs in its own `:tovek_server` process: it loads this library
/// with `System.loadLibrary` and starts/stops the embedded axum server here.
/// No `ndk_context` exists in that process (there is no activity), so these
/// functions must not touch the `android` module — plain Rust only.
#[cfg(target_os = "android")]
mod service_jni {
    use std::sync::Mutex;

    use jni::JNIEnv;
    use jni::objects::{JClass, JString};
    use jni::sys::{jboolean, jint};

    static SERVER: Mutex<Option<crate::jobs::ServerHandle>> = Mutex::new(None);

    fn optional_string(env: &mut JNIEnv, value: &JString) -> Option<String> {
        if value.as_raw().is_null() {
            return None;
        }
        env.get_string(value).ok().map(String::from)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_tovek_gui_TovekService_nativeStartServer(
        mut env: JNIEnv,
        _class: JClass,
        port: jint,
        bind_all: jboolean,
        cert_path: JString,
        key_path: JString,
    ) {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("tovek-server"),
        );
        crate::init_decompiler();
        let port = port as u16;
        let bind_all = bind_all != 0;
        let tls = match (
            optional_string(&mut env, &cert_path),
            optional_string(&mut env, &key_path),
        ) {
            (Some(cert), Some(key)) => Some((cert.into(), key.into())),
            _ => None,
        };
        let mut slot = SERVER.lock().expect("server slot poisoned");
        if let Some(handle) = slot.as_ref() {
            if handle.port == port && handle.bind_all == bind_all {
                log::info!("server already running on port {port}");
                return;
            }
            // Parameters changed: stop the old server before starting anew.
            slot.take();
        }
        log::info!("starting server on port {port} (bind_all={bind_all}, tls={})", tls.is_some());
        *slot = Some(crate::jobs::start_server(port, bind_all, tls));
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_dev_tovek_gui_TovekService_nativeStopServer(
        _env: JNIEnv,
        _class: JClass,
    ) {
        if let Some(mut handle) = SERVER.lock().expect("server slot poisoned").take() {
            log::info!("stopping server");
            handle.stop();
        }
    }
}
