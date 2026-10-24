#!/usr/bin/env bash
# build-android-rust.sh — cross-compile the Rust core for the Android app.
#
# Output: android/app/src/main/jniLibs/arm64-v8a/libsession_digger.so
#
# Degrades to a no-op (with a console note) when the Rust toolchain or
# cargo-ndk is missing, so pure-Kotlin Gradle builds still succeed.
# Set RUST_ANDROID_GRADLE_SKIP=1 to force the no-op.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$SCRIPT_DIR/rust"
OUT_DIR="$SCRIPT_DIR/android/app/src/main/jniLibs/arm64-v8a"
TARGET="aarch64-linux-android"
LIB="libsession_digger.so"

note() { printf '[build-android-rust] %s\n' "$*"; }

if [[ "${RUST_ANDROID_GRADLE_SKIP:-0}" == "1" ]]; then
    note "RUST_ANDROID_GRADLE_SKIP=1 — skipping Rust build"
    exit 0
fi

if ! command -v cargo >/dev/null 2>&1; then
    note "cargo not found — building without the Rust core."
    note "Install Rust (rustup) and cargo-ndk (cargo install cargo-ndk) to embed the digger."
    exit 0
fi

if ! command -v cargo-ndk >/dev/null 2>&1; then
    note "cargo-ndk not found — run: cargo install cargo-ndk"
    note "Skipping native build; Kotlin-only APK will fail at runtime until the .so is present."
    exit 0
fi

# Locate the NDK via ANDROID_NDK_HOME / ANDROID_NDK_ROOT / sdkmanager default.
NDK_CANDIDATE="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
if [[ -z "$NDK_CANDIDATE" ]]; then
    for base in "$HOME/Android/Sdk/ndk" "/opt/android-sdk/ndk"; do
        [[ -d "$base" ]] && NDK_CANDIDATE="$(ls -d "$base"/* 2>/dev/null | sort -V | tail -1)" && break
    done
fi
if [[ -n "$NDK_CANDIDATE" && -d "$NDK_CANDIDATE" ]]; then
    export ANDROID_NDK_HOME="$NDK_CANDIDATE"
else
    note "No NDK found (set ANDROID_NDK_HOME) — cargo-ndk will try its own lookup."
fi

note "cargo-ndk build: $TARGET (release)"
( cd "$RUST_DIR" && cargo ndk -t "$TARGET" -o "$OUT_DIR" build --release ) || {
    note "cargo-ndk build FAILED — check the log above."
    exit 1
}

if [[ -f "$OUT_DIR/$LIB" ]]; then
    note "OK: $OUT_DIR/$LIB"
else
    note "cargo-ndk reported success but $LIB is missing; inspect jniLibs layout."
    exit 1
fi
