//! Android foreground/keep-alive support.
//!
//! The embedded server does NOT run inside the UI process on Android: swiping
//! the app away kills that process no matter what locks it holds. Instead the
//! server runs inside `TovekService` (tovek-gui/android/TovekService.java), a
//! foreground service in its own `:tovek_server` process which loads this
//! same native library and drives it through the `nativeStartServer` /
//! `nativeStopServer` JNI exports in `lib.rs`. The service holds a partial
//! wake lock + WiFi lock and shows a persistent notification with a Stop
//! action — Termux-style. This module is the UI-process side: it fires the
//! start/stop intents, requests the notification permission, and handles the
//! screen/keyboard niceties.

use jni::JavaVM;
use jni::objects::{JObject, JValue};
use winit::platform::android::activity::{AndroidApp, WindowManagerFlags};

const SERVICE_CLASS: &str = "dev.tovek.gui.TovekService";
const ACTION_STOP: &str = "dev.tovek.gui.action.STOP";

/// Run `f` with an attached JNI env + the NativeActivity object.
fn with_activity<T>(
    f: impl FnOnce(&mut jni::JNIEnv, &JObject) -> jni::errors::Result<T>,
) -> Result<T, String> {
    let ctx = ndk_context::android_context();
    // SAFETY: ndk-context is populated by the android-activity glue with a
    // valid JavaVM and NativeActivity pointer for the lifetime of the app.
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }.map_err(|e| e.to_string())?;
    let mut env = vm.attach_current_thread().map_err(|e| e.to_string())?;
    let activity = unsafe { JObject::from_raw(ctx.context() as jni::sys::jobject) };
    let result = f(&mut env, &activity);
    result.map_err(|err| {
        // A pending Java exception would abort the process on the next JNI
        // call. Pull the throwable's toString() so the user sees the real
        // reason (e.g. a SecurityException), not just "Java exception was
        // thrown", then clear it.
        let mut detail = None;
        if env.exception_check().unwrap_or(false) {
            let throwable = env.exception_occurred();
            let _ = env.exception_clear();
            if let Ok(throwable) = throwable {
                detail = env
                    .call_method(&throwable, "toString", "()Ljava/lang/String;", &[])
                    .ok()
                    .and_then(|v| v.l().ok())
                    .and_then(|s| {
                        env.get_string(&jni::objects::JString::from(s))
                            .ok()
                            .map(String::from)
                    });
            }
        }
        match detail {
            Some(detail) => format!("{err}: {detail}"),
            None => err.to_string(),
        }
    })
}

/// Add or clear `FLAG_KEEP_SCREEN_ON` on the activity's window.
///
/// Deliberately NOT done over JNI: `Window.addFlags` touches the view
/// hierarchy, which only the Java UI thread may do — calling it from the
/// native (android_main / render) thread throws
/// `CalledFromWrongThreadException`. `AndroidApp::set_window_flags` goes
/// through the NDK's `ANativeActivity_setWindowFlags`, which posts the change
/// to the UI thread and is safe to call from any thread, at any time.
pub fn keep_screen_on(app: &AndroidApp, on: bool) {
    if on {
        app.set_window_flags(WindowManagerFlags::KEEP_SCREEN_ON, WindowManagerFlags::empty());
    } else {
        app.set_window_flags(WindowManagerFlags::empty(), WindowManagerFlags::KEEP_SCREEN_ON);
    }
}

/// Build an Intent targeting `TovekService` (loaded through the activity's
/// class loader — `FindClass` from a native thread cannot see app classes).
fn service_intent<'a>(
    env: &mut jni::JNIEnv<'a>,
    activity: &JObject,
) -> jni::errors::Result<JObject<'a>> {
    let name = env.new_string(SERVICE_CLASS)?;
    let loader = env
        .call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
        .l()?;
    let class = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&name)],
        )?
        .l()?;
    env.new_object(
        "android/content/Intent",
        "(Landroid/content/Context;Ljava/lang/Class;)V",
        &[JValue::Object(activity), JValue::Object(&class)],
    )
}

/// Start the foreground service that hosts the decompiler server.
/// `tls` carries `(cert_path, key_path)` PEM files for HTTPS, or `None` for
/// plain HTTP.
pub fn start_server_service(
    port: u16,
    bind_all: bool,
    tls: Option<(&str, &str)>,
) -> Result<(), String> {
    with_activity(|env, activity| {
        let intent = service_intent(env, activity)?;
        let key_port = env.new_string("port")?;
        env.call_method(
            &intent,
            "putExtra",
            "(Ljava/lang/String;I)Landroid/content/Intent;",
            &[JValue::Object(&key_port), JValue::Int(i32::from(port))],
        )?;
        let key_bind = env.new_string("bind_all")?;
        env.call_method(
            &intent,
            "putExtra",
            "(Ljava/lang/String;Z)Landroid/content/Intent;",
            &[JValue::Object(&key_bind), JValue::Bool(u8::from(bind_all))],
        )?;
        if let Some((cert, key)) = tls {
            let key_cert = env.new_string("cert_path")?;
            let cert_value = env.new_string(cert)?;
            env.call_method(
                &intent,
                "putExtra",
                "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
                &[JValue::Object(&key_cert), JValue::Object(&cert_value)],
            )?;
            let key_key = env.new_string("key_path")?;
            let key_value = env.new_string(key)?;
            env.call_method(
                &intent,
                "putExtra",
                "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
                &[JValue::Object(&key_key), JValue::Object(&key_value)],
            )?;
        }
        env.call_method(
            activity,
            "startForegroundService",
            "(Landroid/content/Intent;)Landroid/content/ComponentName;",
            &[JValue::Object(&intent)],
        )?;
        Ok(())
    })
}

/// Put `text` on the system clipboard.
///
/// eframe's clipboard backend (arboard) has no Android support, so egui's
/// copy commands silently vanish there. This routes them through the
/// `TovekService.copyToClipboard` Java helper, which posts to the main
/// looper (ClipboardManager must be used from a Looper thread).
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    with_activity(|env, activity| {
        let name = env.new_string(SERVICE_CLASS)?;
        let loader = env
            .call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
            .l()?;
        let class = env
            .call_method(
                &loader,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[JValue::Object(&name)],
            )?
            .l()?;
        let jtext = env.new_string(text)?;
        env.call_static_method(
            jni::objects::JClass::from(class),
            "copyToClipboard",
            "(Landroid/content/Context;Ljava/lang/String;)V",
            &[JValue::Object(activity), JValue::Object(&jtext)],
        )?;
        Ok(())
    })
}

/// Ask the service to stop the server, drop its notification and exit.
pub fn stop_server_service() -> Result<(), String> {
    with_activity(|env, activity| {
        let intent = service_intent(env, activity)?;
        let action = env.new_string(ACTION_STOP)?;
        env.call_method(
            &intent,
            "setAction",
            "(Ljava/lang/String;)Landroid/content/Intent;",
            &[JValue::Object(&action)],
        )?;
        env.call_method(
            activity,
            "startService",
            "(Landroid/content/Intent;)Landroid/content/ComponentName;",
            &[JValue::Object(&intent)],
        )?;
        Ok(())
    })
}

/// Request `POST_NOTIFICATIONS` (API 33+) so the foreground service's
/// notification is visible. The service runs either way — this only controls
/// whether the user can see (and stop) it from the shade.
pub fn request_post_notifications() -> Result<(), String> {
    with_activity(|env, activity| {
        let sdk = env
            .get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?
            .i()?;
        if sdk < 33 {
            return Ok(());
        }
        let permission = env.new_string("android.permission.POST_NOTIFICATIONS")?;
        let granted = env
            .call_method(
                activity,
                "checkSelfPermission",
                "(Ljava/lang/String;)I",
                &[JValue::Object(&permission)],
            )?
            .i()?;
        if granted == 0 {
            return Ok(()); // PERMISSION_GRANTED
        }
        let array = env.new_object_array(1, "java/lang/String", &permission)?;
        env.call_method(
            activity,
            "requestPermissions",
            "([Ljava/lang/String;I)V",
            &[JValue::Object(&array), JValue::Int(0)],
        )?;
        Ok(())
    })
}

/// Is this app exempt from battery optimization (Doze)? Aggressive OEM ROMs
/// kill even foreground services on task swipe unless the app is exempt —
/// the same reason Termux asks users to disable battery optimization.
pub fn is_ignoring_battery_optimizations() -> Result<bool, String> {
    with_activity(|env, activity| {
        let service_name = env.new_string("power")?;
        let power_manager = env
            .call_method(
                activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&service_name)],
            )?
            .l()?;
        let package = env
            .call_method(activity, "getPackageName", "()Ljava/lang/String;", &[])?
            .l()?;
        env.call_method(
            &power_manager,
            "isIgnoringBatteryOptimizations",
            "(Ljava/lang/String;)Z",
            &[JValue::Object(&package)],
        )?
        .z()
    })
}

/// Show the system "let this app run in the background" dialog
/// (ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, backed by the
/// REQUEST_IGNORE_BATTERY_OPTIMIZATIONS permission in manifest.yaml).
pub fn request_ignore_battery_optimizations() -> Result<(), String> {
    with_activity(|env, activity| {
        let action =
            env.new_string("android.settings.REQUEST_IGNORE_BATTERY_OPTIMIZATIONS")?;
        let intent = env.new_object(
            "android/content/Intent",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&action)],
        )?;
        let package = env
            .call_method(activity, "getPackageName", "()Ljava/lang/String;", &[])?
            .l()?;
        let package: String = env
            .get_string(&jni::objects::JString::from(package))?
            .into();
        let uri_text = env.new_string(format!("package:{package}"))?;
        let uri = env
            .call_static_method(
                "android/net/Uri",
                "parse",
                "(Ljava/lang/String;)Landroid/net/Uri;",
                &[JValue::Object(&uri_text)],
            )?
            .l()?;
        env.call_method(
            &intent,
            "setData",
            "(Landroid/net/Uri;)Landroid/content/Intent;",
            &[JValue::Object(&uri)],
        )?;
        env.call_method(
            activity,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[JValue::Object(&intent)],
        )?;
        Ok(())
    })
}

/// Show or hide the soft keyboard (NativeActivity never raises it on its own,
/// so text fields get a manual toggle in the top bar).
pub fn show_soft_input(app: &AndroidApp, show: bool) {
    if show {
        app.show_soft_input(true);
    } else {
        app.hide_soft_input(true);
    }
}
