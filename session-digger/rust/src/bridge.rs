//! Android-side JNI plumbing: enqueue entry point and Java event callbacks.
//!
//! Class contract (see `android/app/src/main/java/com/droidcon/digger/`):
//! - `com.droidcon.digger.NativeBridge` declares the five rivium host methods
//!   plus `static native int nativeEnqueue(String url)`.
//! - `com.droidcon.digger.JobEvents` declares
//!   `static void onJobUpdate(String json)` — called from Rust worker threads.
//!
//! Class-loading rule: Rust threads cannot use `Env::find_class` for app
//! classes, so during `JNI_OnLoad` (running on a Java thread with the app
//! loader) we cache `Global<JClass>` handles and later resolve `JobEvents`
//! through `LoaderContext::FromObject` against the cached bridge class.

use crate::ledger::JobRow;
use jni::{
    objects::{Global, JClass, JString, JValue},
    refs::LoaderContext,
    strings::JNIString,
    {Env, JavaVM},
};
use std::sync::OnceLock;

struct JniRuntime {
    vm: JavaVM,
    bridge: Global<JClass<'static>>,
    events: Global<JClass<'static>>,
}

static JNI_RUNTIME: OnceLock<JniRuntime> = OnceLock::new();

/// Called once from `JNI_OnLoad` (android_export.rs), on a Java thread.
pub fn init(env: &mut Env<'_>) -> jni::errors::Result<()> {
    let vm = JavaVM::singleton().map_err(|e| jni::errors::Error::JniCall(e))?;

    let bridge_local = env.find_class(jni_str!("com/droidcon/digger/NativeBridge"))?;
    let bridge = env.new_global_ref(&bridge_local)?;

    // Resolve the callback class through the bridge class's loader.
    let events_local = LoaderContext::FromObject(bridge.as_ref())
        .find_class(env, jni_str!("com/droidcon/digger/JobEvents"), true)?;
    let events = env.new_global_ref(&events_local)?;

    // Extra native method beyond the five host methods.
    let bindings = [jni::native_method! {
        static fn native_enqueue(env, _class: JClass<'local>, url: JString<'local>) -> jint {
            enqueue_native(env, url)
        }
        name = "nativeEnqueue"
    }];
    env.register_native_methods(
        JNIString::from(jni_str!("com/droidcon/digger/NativeBridge")),
        &bindings,
    )?;

    JNI_RUNTIME
        .set(JniRuntime {
            vm,
            bridge,
            events,
        })
        .map_err(|_| {
            jni::errors::Error::JniCall(jni::errors::JniError::ThrowNew)
        })
}

/// `NativeBridge.nativeEnqueue(String)` implementation.
/// Returns 0 on success, -1 before host startup, -2 when the pump is gone.
fn enqueue_native(env: &mut Env<'_>, url: JString<'_>) -> i32 {
    let url = match env.get_string(&url) {
        Ok(chars) => chars.to_string(),
        Err(_) => return -1,
    };
    match crate::services::shared_state() {
        Some(shared) => {
            if shared.enqueue_tx.send(url).is_ok() {
                0
            } else {
                -2
            }
        }
        None => -1,
    }
}

/// Forward a ledger transition to `JobEvents.onJobUpdate(String json)`.
/// Called on tokio worker threads; attaches on demand.
pub fn emit(row: &JobRow) {
    let Some(runtime) = JNI_RUNTIME.get() else {
        tracing::debug!(id = %row.id, "JNI runtime not initialized; event dropped");
        return;
    };
    let json = match serde_json::to_string(row) {
        Ok(json) => json,
        Err(error) => {
            tracing::warn!(error = %error, "event serialization failed");
            return;
        }
    };
    let result = runtime.vm.attach_current_thread_with_config(
        || {
            let mut config = jni::AttachConfig::new();
            config.exceptions_policy(jni::AttachmentExceptionPolicy::Ignore);
            config
        },
        None,
        |env: &mut Env<'_>| -> jni::errors::Result<()> {
            let java_string = env.new_string(&json)?;
            env.call_static_method(
                &runtime.events,
                jni_str!("onJobUpdate"),
                jni_sig!("(Ljava/lang/String;)V"),
                &[JValue::Object(&java_string)],
            )?;
            Ok(())
        },
    );
    if let Err(error) = result {
        tracing::warn!(error = %error, "onJobUpdate callback failed");
    }
}
