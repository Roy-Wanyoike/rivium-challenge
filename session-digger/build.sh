#!/usr/bin/env bash
# build.sh — reproducible debug-APK build for the `session-digger` submission.
#
# Contract (challenge CI): run from the repository root as
#   bash session-digger/build.sh
# and produce session-digger/debug.apk. Also works when invoked from any
# other working directory, and on a developer laptop.
#
# The submission owns its toolchain, so this script bootstraps whatever a
# bare CI image lacks (Rust stable, the aarch64-linux-android target and
# cargo-ndk), detects the Android SDK/NDK that GitHub's ubuntu runners
# preinstall (or honors explicit ANDROID_HOME / ANDROID_NDK_HOME), then:
#
#   1. cross-compiles the Rust core (libsession_digger.so, arm64-v8a)
#   2. runs the Gradle debug build (Compose app embeds the .so via jniLibs)
#   3. copies the APK to session-digger/debug.apk
#
# Environment knobs: ANDROID_HOME, ANDROID_NDK_HOME, JAVA_HOME (>=17).
# Re-running is safe: every stage is cached/idempotent.
set -euo pipefail

SESSION_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SESSION_DIR"

log() { printf '[session-digger/build] %s\n' "$*"; }
die() { printf '[session-digger/build] ERROR: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# 0. Android SDK + NDK detection (GitHub runners preinstall both; laptops
#    usually export ANDROID_HOME already).
# ---------------------------------------------------------------------------
if [[ -z "${ANDROID_HOME:-}" && -n "${ANDROID_SDK_ROOT:-}" ]]; then
    export ANDROID_HOME="$ANDROID_SDK_ROOT"
fi
if [[ -z "${ANDROID_HOME:-}" ]]; then
    for candidate in /usr/local/lib/android/sdk "$HOME/Android/Sdk" /opt/android-sdk; do
        if [[ -d "$candidate" ]]; then export ANDROID_HOME="$candidate"; break; fi
    done
fi
[[ -n "${ANDROID_HOME:-}" && -d "$ANDROID_HOME" ]] \
    || die "Android SDK not found — set ANDROID_HOME (CI images ship one at /usr/local/lib/android/sdk)."
log "Android SDK: $ANDROID_HOME"

if [[ -z "${ANDROID_NDK_HOME:-}" && -n "${ANDROID_NDK_ROOT:-}" ]]; then
    export ANDROID_NDK_HOME="$ANDROID_NDK_ROOT"
fi
if [[ -z "${ANDROID_NDK_HOME:-}" && -d "$ANDROID_HOME/ndk" ]]; then
    ANDROID_NDK_HOME="$(ls -d "$ANDROID_HOME/ndk"/* 2>/dev/null | sort -V | tail -1 || true)"
    [[ -n "$ANDROID_NDK_HOME" ]] && export ANDROID_NDK_HOME
fi
[[ -n "${ANDROID_NDK_HOME:-}" && -d "$ANDROID_NDK_HOME" ]] \
    || die "Android NDK not found — set ANDROID_NDK_HOME (sdkmanager \"ndk;27.1.12297006\")."
log "Android NDK: $ANDROID_NDK_HOME"

# ---------------------------------------------------------------------------
# 1. JDK 17+ for Gradle/AGP (runner images expose JAVA_HOME_17_X64).
# ---------------------------------------------------------------------------
if [[ -z "${JAVA_HOME:-}" && -n "${JAVA_HOME_17_X64:-}" ]]; then
    export JAVA_HOME="$JAVA_HOME_17_X64"
fi
command -v java >/dev/null 2>&1 || die "No java on PATH — install a JDK 17+."
log "Java: $(java -version 2>&1 | head -1)"

# ---------------------------------------------------------------------------
# 2. Rust toolchain bootstrap (a bare CI image has no rust; laptops usually
#    do). Minimal profile + the arm64 cross target + cargo-ndk.
# ---------------------------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1 && [[ ! -x "$HOME/.cargo/bin/cargo" ]]; then
    log "Installing Rust (rustup, minimal profile)…"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup-init.sh
    bash /tmp/rustup-init.sh -y --profile minimal --default-toolchain stable >/dev/null
fi
[[ -d "$HOME/.cargo/bin" ]] && export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null 2>&1 || die "cargo still missing after bootstrap."
log "Rust: $(rustc --version)"

TARGET="aarch64-linux-android"
rustup target list --installed 2>/dev/null | grep -qx "$TARGET" \
    || rustup target add "$TARGET"
command -v cargo-ndk >/dev/null 2>&1 || {
    log "Installing cargo-ndk (first run only)…"
    cargo install cargo-ndk --locked
}

# ---------------------------------------------------------------------------
# 3. Cross-compile the Rust core into the app's jniLibs (arm64-v8a).
#    Same script a laptop build uses; cargo's cache makes re-runs cheap.
# ---------------------------------------------------------------------------
log "Cross-compiling Rust core ($TARGET, release)…"
bash scripts/build-android-rust.sh

SO="android/app/src/main/jniLibs/arm64-v8a/libsession_digger.so"
[[ -s "$SO" ]] || die "Rust core missing after build: $SO"
log "Rust core OK: $SO ($(wc -c < "$SO") bytes)"

# ---------------------------------------------------------------------------
# 4. Gradle debug build. The .so is already in place, so skip the nested
#    cargo invocation that gradle's preBuild hook would otherwise run.
# ---------------------------------------------------------------------------
log "Gradle assembleDebug (first run downloads Gradle + deps)…"
RUST_ANDROID_GRADLE_SKIP=1 bash android/gradlew -p android --no-daemon assembleDebug

APK_SRC="$(ls android/app/build/outputs/apk/debug/*.apk 2>/dev/null | head -1 || true)"
[[ -n "$APK_SRC" && -s "$APK_SRC" ]] || die "Gradle produced no debug APK."
cp -f "$APK_SRC" "$SESSION_DIR/debug.apk"

# ---------------------------------------------------------------------------
# 5. Sanity check the artifact the CI contract expects.
#    (grep without -q on purpose: -q exits early, SIGPIPEs unzip, and
#    pipefail then fails the check even on a match.)
# ---------------------------------------------------------------------------
APK_LISTING="$(unzip -l "$SESSION_DIR/debug.apk")"
grep -q "lib/arm64-v8a/libsession_digger.so" <<<"$APK_LISTING" \
    || die "debug.apk does not embed libsession_digger.so — check the jniLibs wiring."
grep -q "classes.dex" <<<"$APK_LISTING" \
    || die "debug.apk has no classes.dex — Kotlin build incomplete?"

log "SUCCESS: $SESSION_DIR/debug.apk ($(wc -c < "$SESSION_DIR/debug.apk") bytes)"
