package com.droidcon.digger;

/**
 * JNI surface of the Rust core (libsession_digger.so).
 *
 * The first five statics are the fixed rivium host contract; they are bound
 * by RegisterNatives during JNI_OnLoad (rivium-jni). {@code nativeEnqueue}
 * is the challenge-specific extra method registered by our own JNI_OnLoad
 * hook in rust/src/android_export.rs.
 */
public final class NativeBridge {

    static {
        System.loadLibrary("session_digger");
    }

    private NativeBridge() {
        // Static surface only.
    }

    // rivium host contract — mirrored from rivium-jni's README.
    static native int nativeStart(String[] args);

    static native int nativeStop(long timeoutMillis);

    static native int nativeStatus();

    static native String nativeLastError();

    static native String nativeVersion();

    /** Challenge method: hand a URL to the enqueue pump (0 = ok). */
    static native int nativeEnqueue(String url);
}
