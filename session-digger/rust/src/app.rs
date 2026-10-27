//! The rivium-host application: composition root for the digger.
//!
//! One process, three supervised services (two on Android):
//! `digest-worker` → `enqueue-pump` → `link-ingest` (desktop only).
//! Durable queue state lives in apalis-sqlite under `<root>/data/queue.db`;
//! the digest index (results, titles, hashes) in `<root>/data/jobs.db`.

use crate::jobs::{DigestJob, Speaker};
use crate::ledger::{JobRow, Ledger};
use crate::services::{self, EventSink, Shared, SpeakerSink, WorkerBackend};
use apalis_core::backend::ext::BackendExt;
use apalis_sqlite::{Config as QueueConfig, SqliteStorage};
use rivium::{Error, Result, Service};
use rivium_host::Context as HostContext;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;

/// Runtime tuning, overridable via `--set concurrency=2` and friends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Parallel digests inside one worker.
    pub concurrency: usize,
    /// Per-request wall-clock budget.
    pub request_timeout_secs: u64,
    /// Sent with every fetch; droidcon-hosted pages should recognize us.
    pub user_agent: String,
    /// Desktop only (requires the cargo `board` feature): bind address for
    /// the apalis-board queue inspector UI. Empty = disabled.
    /// Example: `cargo run --features board --bin digd -- --root ./demo-root
    /// --set board_addr=127.0.0.1:8080`.
    #[serde(default)]
    pub board_addr: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            concurrency: 4,
            request_timeout_secs: 20,
            user_agent: concat!(
                "session-digger/",
                env!("CARGO_PKG_VERSION"),
                " (droidcon Kenya challenge)"
            )
            .to_string(),
            board_addr: String::new(),
        }
    }
}

static STATE: OnceLock<Arc<Shared>> = OnceLock::new();

/// Background runtime for sqlx pool construction and its maintenance task:
/// `App::services` runs on the host thread *outside* any tokio context, but
/// `SqlitePool` spawns its reaper at creation time and needs one. Kept alive
/// for the process lifetime so the pool's maintenance task keeps ticking.
static QUEUE_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Shared handle for the task handler and JNI bridge; installed by
/// `services` before any service future runs.
pub fn state() -> Option<Arc<Shared>> {
    STATE.get().cloned()
}

/// Open the durable apalis-sqlite queue at `<root>/data/queue.db`, run its
/// migrations, and return the push-side storage plus the hook listener that
/// wakes the worker the instant the database changes.
fn open_queue(
    root: &std::path::Path,
) -> Result<(SqliteStorage<DigestJob>, WorkerBackend)> {
    std::fs::create_dir_all(root)
        .map_err(|e| Error::internal(format!("queue dir: {e}")))?;
    let url = format!("sqlite://{}/queue.db?mode=rwc", root.display());
    let runtime = QUEUE_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("queue runtime")
    });
    let build = async {
        let (pool, listener) = SqliteStorage::connect_with_callback(&url)
            .map_err(|e| Error::internal(format!("queue connect: {e}")))?;
        SqliteStorage::<()>::setup(&pool)
            .await
            .map_err(|e| Error::internal(format!("queue migrate: {e}")))?;
        Ok::<_, Error>((pool, listener))
    };
    // block_on is only reached from a sync (non-async) host thread; rivium
    // calls `App::services` before its own runtime is entered.
    let (pool, listener) = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(build)?,
        Err(_) => runtime.block_on(build)?,
    };
    // Heartbeat 5s (orphan timeout 10s): tasks left locked by a dead worker
    // are re-enqueued by the live worker's heartbeat sweep within seconds of
    // the crash; the register-time sweep handles any that outlived it.
    let config = QueueConfig::default()
        .queue("digest")
        .heartbeat_interval(Duration::from_secs(5));
    let storage = SqliteStorage::<DigestJob>::new(&pool).with_config(config);
    let worker_backend = storage.clone().poll_with_stream(listener);
    Ok((storage, worker_backend))
}

/// Application marker type used by both the desktop `digd` binary and the
/// Android JNI export.
pub struct DiggerApp;

impl rivium_host::App for DiggerApp {
    const NAME: &'static str = "digd";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");
    type Config = Config;

    fn validate(config: &Self::Config) -> Result<()> {
        if config.concurrency == 0 || config.concurrency > 16 {
            return Err(Error::invalid_input("concurrency must be 1..=16"));
        }
        if config.request_timeout_secs == 0 || config.request_timeout_secs > 300 {
            return Err(Error::invalid_input("request_timeout_secs must be 1..=300"));
        }
        if config.user_agent.trim().is_empty() {
            return Err(Error::invalid_input("user_agent must not be empty"));
        }
        if !config.board_addr.is_empty() {
            if let Err(e) = config.board_addr.parse::<std::net::SocketAddr>() {
                return Err(Error::invalid_input(format!(
                    "board_addr must be a socket address (e.g. 127.0.0.1:8080): {e}"
                )));
            }
        }
        Ok(())
    }

    fn services(config: &Self::Config, ctx: &HostContext<Self::Config>) -> Result<Vec<Box<dyn Service>>> {
        let root = ctx.paths().root().join("data");
        let ledger =
            Ledger::open(&root).map_err(|e| Error::internal(format!("ledger open: {e}")))?;
        let (queue, worker_backend) = open_queue(&root)?;
        // The board inspects the durable queue itself, so hand it a clone of
        // the same storage handle the pump pushes through (taken before the
        // move into `Shared`).
        #[cfg(all(feature = "board", not(target_os = "android")))]
        let board_queue = queue.clone();

        // Transport channel: raw URL strings from stdin (desktop) or JNI.
        let (enqueue_tx, enqueue_rx) = mpsc::unbounded_channel();

        let events: EventSink = Arc::new(emit_row);
        let speakers_event: SpeakerSink = Arc::new(emit_speakers);

        let shared = Arc::new(Shared {
            ledger: Arc::new(ledger),
            enqueue_tx,
            queue,
            events,
            speakers_event,
            concurrency: config.concurrency,
            request_timeout: Duration::from_secs(config.request_timeout_secs),
            user_agent: config.user_agent.clone(),
        });
        STATE
            .set(shared.clone())
            .map_err(|_| Error::internal("digger state already initialized in this process"))?;

        let mut roots: Vec<Box<dyn Service>> =
            vec![services::digest_worker(shared.clone(), worker_backend)];
        #[cfg(not(target_os = "android"))]
        {
            let (lines_tx, lines_rx) = mpsc::unbounded_channel();
            roots.push(services::link_ingest(lines_tx, lines_rx));
        }
        roots.push(services::enqueue_pump(shared, enqueue_rx));
        // Opt-in desktop queue inspector (apalis-board); see `Config::board_addr`.
        #[cfg(all(feature = "board", not(target_os = "android")))]
        if !config.board_addr.is_empty() {
            let addr = config
                .board_addr
                .parse::<std::net::SocketAddr>()
                .map_err(|e| Error::internal(format!("board_addr: {e}")));
            roots.push(services::board_server(board_queue, addr?));
        }
        Ok(roots)
    }
}

/// Every ledger transition funnels through here.
fn emit_row(row: &JobRow) {
    #[cfg(target_os = "android")]
    crate::bridge::emit(row);

    #[cfg(not(target_os = "android"))]
    {
        let detail = match (row.state.as_str(), &row.title, &row.sha256, &row.error) {
            ("done", Some(title), Some(sha256), _) => {
                format!("title={title:?} sha256={sha256}")
            }
            (_, _, _, Some(error)) => format!("error={error}"),
            _ => String::new(),
        };
        println!("digd [{:>7}] {} {}", row.state, row.url, detail);
    }
}

/// A harvested speaker list funnels through here.
fn emit_speakers(url: &str, speakers: &[Speaker]) {
    #[cfg(target_os = "android")]
    crate::bridge::emit_speakers(url, speakers);

    #[cfg(not(target_os = "android"))]
    {
        println!("digd [{:>7}] {} harvested {} speakers", "Done", url, speakers.len());
        for speaker in speakers {
            println!(
                "  · {} — {}",
                speaker.name,
                speaker.role.as_deref().unwrap_or("")
            );
        }
    }
}
