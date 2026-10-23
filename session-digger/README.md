# session-digger — droidcon Kenya link digger

A **Rivium** application that digests droidcon.co.ke links: fetches a URL,
extracts the page title, and records its **SHA-256** — with crash-safe
persistence and a Material 3 Android front-end including a **Quick Settings
tile**.

```
QS tile ─┐                          ┌─> notifications
Compose ─┼─> NativeBridge.nativeEnqueue (JNI)
stdin ───┘            │
                      v
             enqueue channel (tokio mpsc)
                      │
             ┌── enqueue-pump ─────────────┐
             │  SQLite ledger = truth      │   replay queued/running
             └──────────┬──────────────────┘   rows on boot (SIGKILL-safe)
                       v
             ChannelBackend ──> apalis worker
             (parallelize -> concurrency(4) -> retry x2)
                       │
             reqwest GET -> <title> + SHA-256
                       │
             ledger.mark_done -> JobEvents.onJobUpdate (JNI callback)
```

## Layout

| Path | What |
| --- | --- |
| `rust/` | Rivium workspace: ledger, queue backend, digest jobs, services, JNI bridge |
| `rust/src/bin/digd.rs` | Desktop daemon (`digd --root <dir>`, URLs on stdin) |
| `android/` | Gradle project: Compose UI, `DigTileService`, `RustHost`, `JobEvents` |
| `scripts/build-android-rust.sh` | `cargo-ndk` arm64-v8a build hooked into `preBuild` |
| `docs/device-testing.md` | 8-step real-device checklist with logcat filters |
| `docs/demo-script.md` | 90-second screen-recording script |

## Desktop quick start

```bash
cd rust
cargo run --bin digd -- --root ./demo-root
# paste URLs, one per line:
#   https://droidcon.co.ke
#   !https://droidcon.co.ke/broken   ← re-queue a failed job
# Ctrl-D (EOF) stops gracefully
```

Results land in `demo-root/data/jobs.db` (SQLite, WAL):

```sql
SELECT url, state, title, sha256, attempts, error FROM jobs;
```

## Android quick start

1. Install Rust + `cargo install cargo-ndk`, plus an NDK (`ANDROID_NDK_HOME`).
2. `cd android && ./gradlew :app:assembleDebug`
   — Gradle runs `scripts/build-android-rust.sh` via the `preBuild` hook and
   packages `libsession_digger.so` (arm64-v8a). Without a Rust toolchain the
   script degrades to a no-op so Kotlin-only builds still compile.
3. Install on a device, allow notifications, then either:
   - tap **Dig** in the app, or
   - pull down Quick Settings, add the **Digger** tile, tap it.

## Persistence guarantee

SQLite (`jobs.db`) is the single source of truth; the in-process task
channel is only transport. On boot the pump resets crash-orphaned `running`
rows to `queued` and replays every un-finished job — verified by
`rust/tests/laptop.rs::e2e_sigkill_replay` (SIGKILL mid-fetch, restart,
same URL completes with the same hash).

## Test & verify

```bash
cd rust
cargo test                        # unit tests (ledger, entity decoding)
cargo test --test laptop -- --ignored   # live e2e vs droidcon.co.ke
cargo clippy --all-targets        # zero warnings
```

## Rivium usage map (challenge scoring)

| Requirement | Where |
| --- | --- |
| Embedded host on Android | `rust/src/android_export.rs` (`JNI_OnLoad`) + `rivium-jni` five-method contract |
| `App` trait composition | `rust/src/app.rs` (`DiggerApp`: Config/validate/services) |
| Supervised services | `rust/src/services.rs` (`digest-worker`, `enqueue-pump`, `link-ingest`) |
| Custom backend | `rust/src/queue.rs` (`ChannelBackend` over tokio mpsc; apalis `Ephemeral` kind) |
| apalis layers, in order | backend → `parallelize(tokio::spawn)` → `concurrency(n)` → `retry(RetryPolicy::retries(2))` |
| Logs under `<root>/logs/` | written by the rivium host automatically |
| Strict CLI (`--root/--config/--set`) | provided by `rivium_host::process::run` |

## Versions

- Rust 1.99.1 · rivium 0.2.0 · apalis 1.0.0 · jni 0.22.4 · rusqlite 0.32 (bundled)
- AGP 8.7.3 · Kotlin 2.0.21 · Compose BOM 2024.12.01 · compileSdk 35 · minSdk 26 · arm64-v8a
