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
    jni_sig, jni_str,
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
    let vm = JavaVM::singleton()?;

    let bridge_local = env.find_class(jni_str!("com/droidcon/digger/NativeBridge"))?;
    let bridge = env.new_global_ref(&bridge_local)?;

    // Resolve the callback class through the bridge class's loader.
    let events_local = LoaderContext::FromObject(bridge.as_ref())
        .find_class(env, jni_str!("com/droidcon/digger/JobEvents"), true)?;
    let events = env.new_global_ref(&events_local)?;

    // Extra native method beyond the five host methods.
    let bindings = [jni::native_method! {
        static fn native_enqueue(url: JString) -> jint,
        fn = enqueue_native,
        name = "nativeEnqueue",
    }];
    // # Safety: the bindings' fn pointers are valid for the JVM's lifetime —
    // they point at code registered inside this library.
    unsafe {
        env.register_native_methods(
            JNIString::from(jni_str!("com/droidcon/digger/NativeBridge")),
            &bindings,
        )?;
    }

    JNI_RUNTIME
        .set(JniRuntime {
            vm,
            bridge,
            events,
        })
        .map_err(|_| {
            // The runtime is process-wide; a second init means the library
            // was loaded twice.
            jni::errors::Error::JniCall(jni::errors::JniError::AlreadyCreated)
        })
}

/// `NativeBridge.nativeEnqueue(String)` implementation.
/// Returns 0 on success, -1 before host startup, -2 when the pump is gone.
fn enqueue_native(
    _env: &mut Env<'_>,
    _class: JClass<'_>,
    url: JString<'_>,
) -> Result<i32, jni::errors::Error> {
    // JString's Display reads through the VM singleton; on this Java thread
    // the attachment is a no-op.
    let url = url.to_string();
    Ok(match crate::services::shared_state() {
        Some(shared) if shared.enqueue_tx.send(url).is_ok() => 0,
        Some(_) => -2,
        None => -1,
    })
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
    call_events(runtime, Method::JobUpdate, &json);
}

/// Forward a harvested speaker list to `JobEvents.onSpeakers(String json)`.
/// Payload: `{"url": …, "speakers": [{"name": …, "role": …}, …]}`.
pub fn emit_speakers(url: &str, speakers: &[crate::jobs::Speaker]) {
    let Some(runtime) = JNI_RUNTIME.get() else {
        tracing::debug!(count = speakers.len(), "JNI runtime not initialized; speakers dropped");
        return;
    };
    let payload = serde_json::json!({ "url": url, "speakers": speakers }).to_string();
    call_events(runtime, Method::Speakers, &payload);
}

/// The `JobEvents` static callbacks Rust may invoke.
#[derive(Clone, Copy)]
enum Method {
    JobUpdate,
    Speakers,
}

/// Invoke one `JobEvents` static callback with a single String argument.
fn call_events(runtime: &JniRuntime, method: Method, json: &str) {
    let name = match method {
        Method::JobUpdate => "onJobUpdate",
        Method::Speakers => "onSpeakers",
    };
    let result = runtime.vm.attach_current_thread_with_config(
        // Ignore pending JVM exceptions: this callback must never panic back
        // into the FFI wrapper.
        || jni::AttachConfig::new().exceptions_policy(jni::AttachmentExceptionPolicy::Ignore),
        None,
        |env: &mut Env<'_>| -> jni::errors::Result<()> {
            let java_string = env.new_string(json)?;
            // jni_str! needs literals, hence the match per callback.
            let call = match method {
                Method::JobUpdate => env.call_static_method(
                    &runtime.events,
                    jni_str!("onJobUpdate"),
                    jni_sig!("(Ljava/lang/String;)V"),
                    &[JValue::Object(&java_string)],
                ),
                Method::Speakers => env.call_static_method(
                    &runtime.events,
                    jni_str!("onSpeakers"),
                    jni_sig!("(Ljava/lang/String;)V"),
                    &[JValue::Object(&java_string)],
                ),
            };
            call.map(|_| ())
        },
    );
    if let Err(error) = result {
        tracing::warn!(error = %error, name, "JobEvents callback failed");
    }
}
