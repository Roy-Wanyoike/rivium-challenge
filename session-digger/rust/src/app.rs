//! The rivium-host application: composition root for the digger.
//!
//! One process, three supervised services (two on Android):
//! `digest-worker` → `enqueue-pump` → `link-ingest` (desktop only).
//! All durable state lives in the SQLite ledger under `<root>/data`.

use crate::ledger::{JobRow, Ledger};
use crate::queue::ChannelBackend;
use crate::services::{self, EventSink, Shared};
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
        }
    }
}

static STATE: OnceLock<Arc<Shared>> = OnceLock::new();

/// Shared handle for the task handler and JNI bridge; installed by
/// `services` before any service future runs.
pub fn state() -> Option<Arc<Shared>> {
    STATE.get().cloned()
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
        Ok(())
    }

    fn services(config: &Self::Config, ctx: &HostContext<Self::Config>) -> Result<Vec<Box<dyn Service>>> {
        let root = ctx.paths().root().join("data");
        let ledger =
            Ledger::open(&root).map_err(|e| Error::internal(format!("ledger open: {e}")))?;

        // Transport channels: raw URL strings in, ledger-backed tasks out.
        let (enqueue_tx, enqueue_rx) = mpsc::unbounded_channel();
        let (backend, worker_tx) = ChannelBackend::channel();

        let events: EventSink = Arc::new(emit_row);

        let shared = Arc::new(Shared {
            ledger: Arc::new(ledger),
            enqueue_tx,
            worker_tx,
            events,
            concurrency: config.concurrency,
            request_timeout: Duration::from_secs(config.request_timeout_secs),
            user_agent: config.user_agent.clone(),
        });
        STATE
            .set(shared.clone())
            .map_err(|_| Error::internal("digger state already initialized in this process"))?;

        let mut roots: Vec<Box<dyn Service>> =
            vec![services::digest_worker(shared.clone(), backend)];
        #[cfg(not(target_os = "android"))]
        {
            let (lines_tx, lines_rx) = mpsc::unbounded_channel();
            roots.push(services::link_ingest(lines_tx, lines_rx));
        }
        roots.push(services::enqueue_pump(shared, enqueue_rx));
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
