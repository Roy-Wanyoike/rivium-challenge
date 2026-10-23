//! Library entry point for Android: `JNI_OnLoad`.
//!
//! One native library exports exactly one `JNI_OnLoad`. The five rivium host
//! methods are registered through `rivium_jni`'s registration routine; this
//! module then caches the app-loader class handles and registers the extra
//! `nativeEnqueue` method on the same bridge class.

use rivium_jni::__private as rivium_register;

const BRIDGE_CLASS: &str = "com/droidcon/digger/NativeBridge";

/// # Safety
/// `vm` must be the JVM-supplied pointer passed by the ART loader.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn JNI_OnLoad(
    vm: *mut jni::sys::JavaVM,
    _reserved: *mut core::ffi::c_void,
) -> jni::sys::jint {
    // 1. Register the five host natives and initialize the embedded host.
    let registered =
        unsafe { rivium_register::register::<crate::app::DiggerApp>(vm, BRIDGE_CLASS) };
    if registered == jni::sys::JNI_ERR {
        return jni::sys::JNI_ERR;
    }

    // 2. Cache class globals and register nativeEnqueue on the same class.
    let machine = match unsafe { jni::JavaVM::from_raw(vm) } {
        Ok(machine) => machine,
        Err(_) => return jni::sys::JNI_ERR,
    };
    let attached = machine.attach_current_thread_with_config(
        || {
            let mut config = jni::AttachConfig::new();
            config.exceptions_policy(jni::AttachmentExceptionPolicy::Ignore);
            config
        },
        None,
        |env: &mut jni::Env<'_>| crate::bridge::init(env),
    );
    match attached {
        Ok(Ok(())) => registered,
        _ => jni::sys::JNI_ERR,
    }
}
