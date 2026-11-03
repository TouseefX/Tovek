//! Android foreground/keep-alive support, via JNI against the NativeActivity.
//!
//! Two mechanisms, both best-effort (errors are reported, never fatal):
//!   * `FLAG_KEEP_SCREEN_ON` on the activity window — the screen stays awake
//!     while the user leaves the app in the foreground.
//!   * a `PARTIAL_WAKE_LOCK` from `PowerManager`, held while the embedded
//!     decompiler server runs, so the CPU keeps serving an on-device executor
//!     even if the screen dims (requires `android.permission.WAKE_LOCK`,
//!     declared in manifest.yaml).
//!
//! Note: without a Java foreground *service* the OS may still freeze the
//! process once the app leaves the foreground entirely; the supported workflow
//! is to keep Tovek in the foreground (or split-screen) while dumping.

use jni::JavaVM;
use jni::objects::{GlobalRef, JObject, JValue};

const FLAG_KEEP_SCREEN_ON: i32 = 0x0000_0080;
const PARTIAL_WAKE_LOCK: i32 = 0x0000_0001;

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
    // A pending Java exception would abort the process on the next JNI call:
    // clear it and surface the error as a string instead.
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
    result.map_err(|e| e.to_string())
}

/// Add or clear `FLAG_KEEP_SCREEN_ON` on the activity's window.
pub fn keep_screen_on(on: bool) -> Result<(), String> {
    with_activity(|env, activity| {
        let window = env
            .call_method(activity, "getWindow", "()Landroid/view/Window;", &[])?
            .l()?;
        let method = if on { "addFlags" } else { "clearFlags" };
        env.call_method(&window, method, "(I)V", &[JValue::Int(FLAG_KEEP_SCREEN_ON)])?;
        Ok(())
    })
}

/// A held `PowerManager.WakeLock`; released on [`WakeLock::release`] or drop.
pub struct WakeLock {
    lock: GlobalRef,
    released: bool,
}

/// Acquire a partial wake lock named `tovek:server`.
pub fn acquire_wake_lock() -> Result<WakeLock, String> {
    let lock = with_activity(|env, activity| {
        let service_name = env.new_string("power")?;
        let power_manager = env
            .call_method(
                activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&service_name)],
            )?
            .l()?;
        let tag = env.new_string("tovek:server")?;
        let wake_lock = env
            .call_method(
                &power_manager,
                "newWakeLock",
                "(ILjava/lang/String;)Landroid/os/PowerManager$WakeLock;",
                &[JValue::Int(PARTIAL_WAKE_LOCK), JValue::Object(&tag)],
            )?
            .l()?;
        // One acquire()/release() pair regardless of how often we toggle.
        env.call_method(&wake_lock, "setReferenceCounted", "(Z)V", &[JValue::Bool(0)])?;
        env.call_method(&wake_lock, "acquire", "()V", &[])?;
        env.new_global_ref(wake_lock)
    })?;
    Ok(WakeLock {
        lock,
        released: false,
    })
}

impl WakeLock {
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let _ = with_activity(|env, _| {
            env.call_method(self.lock.as_obj(), "release", "()V", &[])?;
            Ok(())
        });
    }
}

impl Drop for WakeLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// Show or hide the soft keyboard (NativeActivity never raises it on its own,
/// so text fields get a manual toggle in the top bar).
pub fn show_soft_input(app: &winit::platform::android::activity::AndroidApp, show: bool) {
    if show {
        app.show_soft_input(true);
    } else {
        app.hide_soft_input(true);
    }
}
