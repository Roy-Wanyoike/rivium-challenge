# Device testing checklist (arm64-v8a real device)

Run these in order after `./gradlew :app:installDebug`. Every step lists the
expected observation; failures point at the most likely cause.

## 0. Prerequisites

- [ ] Rust toolchain + `cargo install cargo-ndk`
- [ ] NDK installed; `ANDROID_NDK_HOME` exported, or a default
      `$HOME/Android/Sdk/ndk/<ver>` present
- [ ] Device: arm64-v8a, Android 8.0+ (minSdk 26), USB debugging on
- [ ] `adb logcat -c` for a clean slate

## 1. Native build hook

```bash
cd android && ./gradlew :app:assembleDebug --info | grep build-android-rust
```

- [ ] Expect `[build-android-rust] OK: .../jniLibs/arm64-v8a/libsession_digger.so`
- If instead you see the no-op note, `cargo`/`cargo-ndk` is missing from
  Gradle's PATH — run once manually:
  `bash scripts/build-android-rust.sh`

## 2. Library load + host start

```bash
adb install -r app/build/outputs/apk/debug/app-debug.apk
adb logcat | grep -E "JobEvents|RustHost|digd|UnsatisfiedLink"
```

- [ ] Launch "Session Digger". No `UnsatisfiedLinkError`; rivium host logs
      land in `/data/data/com.droidcon.digger/files/logs/` — pull with
      `adb shell run-as com.droidcon.digger ls files/logs` (or view in
      Device Explorer).

## 3. Notification permission

- [ ] Accept the POST_NOTIFICATIONS prompt (Android 13+). Denying is fine —
      the app keeps working without result notifications.

## 4. In-app enqueue → digest → UI refresh

- [ ] Tap **Dig** with `https://droidcon.co.ke`.
- Expect a card cycling `QUEUED → RUNNING → DONE` with the real page title
  and a 64-hex sha256 within seconds.

## 5. QS tile

- Pull Quick Settings → edit → add **Digger** tile.
- [ ] Tile is ACTIVE with subtitle `tap to dig droidcon.co.ke`.
- Tap it, reopen the app:
  [ ] A new job appears without touching the app (JNI enqueue path).
- [ ] Subtitle flips to `N in queue` while work is pending.

## 6. Notifications

- [ ] With the app backgrounded, enqueue a URL; on DONE you get a
      "digger: done" notification with the title.

## 7. Crash-safe replay (the money shot)

- Enqueue a URL and immediately `adb shell am force-stop com.droidcon.digger`.
- Relaunch:
  - [ ] The orphaned job resumes (`queued → running → done`) — the pump
        replayed it from `files/data/jobs.db`.
- Verify: `adb shell run-as com.droidcon.digger ls -la files/data/`

## 8. Airplane mode → retry → failed state

- [ ] Enable airplane mode, enqueue a URL: the row exhausts 3 attempts
      (1 initial + 2 retries), state `FAILED`, error text shown on the card.
- [ ] Disable airplane mode. On-device re-queue of a `failed` row is a no-op
      by design (URL idempotency, `Admit::AlreadyFailed`); use the desktop
      `!URL` line to requeue, or clear app data for a clean slate.
