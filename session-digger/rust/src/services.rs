//! Rivium services: the digest worker, the enqueue pump, and (desktop) the
//! stdin link ingest.
//!
//! Wiring recap:
//! - `link-ingest` (desktop only) reads URL lines from stdin and pushes raw
//!   strings into the process-wide enqueue channel (Android pushes the same
//!   channel from JNI via `NativeBridge.nativeEnqueue`).
//! - `enqueue-pump` owns that channel's receiving end, admits URLs into the
//!   SQLite ledger, replays un-finished rows at boot, and forwards
//!   ledger-backed tasks into the worker channel.
//! - `digest-worker` runs the apalis worker over `ChannelBackend` until the
//!   rivium stop token fires, then drains.
//!
//! The SQLite ledger is the source of truth; channels are only transport.

use crate::jobs::{self, DigestJob};
use crate::ledger::{Admit, JobRow, Ledger};
use crate::queue::ChannelBackend;
use apalis::layers::retry::RetryPolicy;
use apalis::layers::WorkerBuilderExt;
use apalis_core::error::WorkerError;
use apalis_core::task::Task;
use apalis_core::task::attempt::Attempt;
use apalis_core::task::builder::TaskBuilder;
use apalis_core::task::context::TaskContext;
use apalis_core::task::task_fn::task_fn;
use apalis_core::task::task_id::TaskId;
use apalis_core::worker::builder::WorkerBuilder;
use apalis_core::worker::ext::parallelize::ParallelizeExt;
use rivium::{Context, Error, Kind, Service, service};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Total attempts per job: 1 initial + 2 retries.
pub const MAX_ATTEMPTS: u32 = 3;
const MAX_RETRIES: usize = (MAX_ATTEMPTS - 1) as usize;

/// Quiescence poll interval and cap for the desktop EOF stop. The cap only
/// guards against a wedged queue; un-finished rows replay on the next boot
/// either way, so a timeout never loses acknowledged work.
const QUIESCE_POLL: Duration = Duration::from_millis(100);
const QUIESCE_POLLS: u32 = 3_000; // 5 minutes

/// Callback invoked on every ledger transition; desktop prints, Android
/// forwards to `JobEvents.onJobUpdate` over JNI.
pub type EventSink = Arc<dyn Fn(&JobRow) + Send + Sync>;

/// Everything the services and the task handler need.
pub struct Shared {
    pub ledger: Arc<Ledger>,
    /// Raw URLs coming from stdin (desktop) or JNI (Android).
    pub enqueue_tx: mpsc::UnboundedSender<String>,
    /// Ledger-backed tasks flowing into the apalis worker.
    pub worker_tx: crate::queue::ChannelHandle<DigestJob>,
    pub events: EventSink,
    pub concurrency: usize,
    pub request_timeout: Duration,
    pub user_agent: String,
}

/// Access the process state installed by `App::services` (the task handler
/// cannot capture anything through the apalis `task_fn`).
pub fn shared_state() -> Option<Arc<Shared>> {
    crate::app::state()
}

fn build_task(row: &JobRow) -> Task<DigestJob> {
    TaskBuilder::new(DigestJob {
        id: row.id.clone(),
        url: row.url.clone(),
    })
    .task_id(TaskId::from_string(row.id.clone()))
    .idempotency_key(row.id.clone())
    .build()
}

/// Replay every un-finished ledger row into the worker channel.
/// `running` rows from a crashed prior run are reset to `queued` first —
/// this is the SIGKILL-recovery path.
fn replay_pending(shared: &Shared) -> rivium::Result<()> {
    shared
        .ledger
        .reset_running()
        .map_err(|e| Error::internal(format!("ledger reset: {e}")))?;
    let pending = shared
        .ledger
        .pending()
        .map_err(|e| Error::internal(format!("ledger read: {e}")))?;
    for row in &pending {
        shared
            .worker_tx
            .send(build_task(row))
            .map_err(|_| Error::internal("worker channel closed during replay"))?;
        (shared.events)(row);
    }
    if !pending.is_empty() {
        tracing::info!(count = pending.len(), "replayed pending jobs");
    }
    Ok(())
}

/// Admission loop: raw URL strings in, ledger-backed tasks out.
pub fn enqueue_pump(
    shared: Arc<Shared>,
    mut enqueue_rx: mpsc::UnboundedReceiver<String>,
) -> Box<dyn Service> {
    service("enqueue-pump", Kind::Frontline, move |ctx: Context| async move {
        replay_pending(&shared)?;
        ctx.ready();
        loop {
            tokio::select! {
                biased;
                _ = ctx.stopped() => {
                    // Graceful stop: drain whatever is still buffered (e.g. a
                    // last-millisecond JNI enqueue on Android) so it is
                    // persisted now and replayed on the next boot.
                    while let Ok(line) = enqueue_rx.try_recv() {
                        admit_line(&shared, &line);
                    }
                    return Ok(());
                }
                maybe_url = enqueue_rx.recv() => match maybe_url {
                    Some(line) => admit_line(&shared, &line),
                    None => {
                        // Every enqueue source is gone: nothing new can arrive.
                        ctx.control().stop(rivium::StopReason::Requested);
                        return Ok(());
                    }
                },
            }
        }
    })
}

/// Wait until every acknowledged job has reached a terminal state (`done` or
/// `failed`). Called on desktop stdin EOF so `printf urls | digd` never exits
/// before the queue is quiet. Capped; see [`QUIESCE_POLLS`].
async fn wait_for_quiescence(shared: &Shared) {
    for _ in 0..QUIESCE_POLLS {
        match shared.ledger.pending() {
            Ok(pending) if pending.is_empty() => return,
            Ok(_) => {}
            // Ledger trouble: don't block teardown.
            Err(_) => return,
        }
        tokio::time::sleep(QUIESCE_POLL).await;
    }
    tracing::warn!(
        "queue did not drain before stop; un-finished rows will replay on next boot"
    );
}

/// `URL` enqueues; `!URL` re-queues a previously failed job; `#...` comments.
fn admit_line(shared: &Shared, line: &str) {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return;
    }
    let (url, requeue) = match line.strip_prefix('!') {
        Some(rest) => (rest.trim(), true),
        None => (line, false),
    };
    if !url.starts_with("http://") && !url.starts_with("https://") {
        tracing::warn!(url, "not an http(s) URL; ignored");
        return;
    }

    if requeue {
        match shared.ledger.requeue_failed(url) {
            Ok(Some(row)) => {
                (shared.events)(&row);
                if let Err(e) = shared.worker_tx.send(build_task(&row)) {
                    tracing::error!(url, error = %e, "worker channel refused task");
                }
            }
            Ok(None) => tracing::info!(url, "nothing to requeue"),
            Err(e) => tracing::error!(url, error = %e, "requeue failed"),
        }
        return;
    }

    match shared.ledger.admit(url) {
        Ok(Admit::Admitted) => {
            let id = crate::ledger::job_id_for(url);
            match shared.ledger.get(&id) {
                Ok(Some(row)) => {
                    (shared.events)(&row);
                    if let Err(e) = shared.worker_tx.send(build_task(&row)) {
                        tracing::error!(url, error = %e, "worker channel refused task");
                    }
                }
                Ok(None) => tracing::error!(url, "admitted row vanished"),
                Err(e) => tracing::error!(url, error = %e, "ledger read failed"),
            }
        }
        Ok(other) => {
            // Sync the observer even when skipping, then leave state intact.
            let id = crate::ledger::job_id_for(url);
            if let Ok(Some(row)) = shared.ledger.get(&id) {
                (shared.events)(&row);
            }
            tracing::info!(url, state = ?other, "job already tracked; skipping");
        }
        Err(e) => tracing::error!(url, error = %e, "ledger admit failed"),
    }
}

/// The apalis worker as a supervised rivium service.
pub fn digest_worker(
    shared: Arc<Shared>,
    backend: ChannelBackend<DigestJob>,
) -> Box<dyn Service> {
    service("digest-worker", Kind::Frontline, move |ctx: Context| async move {
        ctx.ready();
        let concurrency = shared.concurrency.clamp(1, 16);
        let policy = RetryPolicy::retries(MAX_RETRIES);
        let worker = WorkerBuilder::new("dig-worker")
            .backend(backend)
            // parallelize must wrap the raw task_fn first: its future has to be
            // 'static before the concurrency/retry towers see it.
            .parallelize(tokio::spawn)
            .concurrency(concurrency)
            .retry(policy)
            .build(task_fn(run_digest));
        let stop = async move {
            ctx.stopped().await;
            Ok::<(), WorkerError>(())
        };
        worker
            .run_until(stop)
            .await
            .map_err(|e| Error::internal(format!("worker: {e}")))?;
        Ok(())
    })
}

/// The digest task handler, executed (and retried) by the apalis worker.
async fn run_digest(
    job: DigestJob,
    _ctx: TaskContext,
    attempt: Attempt,
) -> std::result::Result<(), jobs::JobError> {
    let shared = shared_state().expect("shared state installed by App::services");
    // apalis attempts are 1-based: the first run reports attempt 1.
    let attempt_no = attempt.current() as u32;

    match shared.ledger.mark_running(&job.id, attempt_no) {
        Ok(row) => (shared.events)(&row),
        Err(e) => tracing::warn!(id = %job.id, error = %e, "ledger mark_running failed"),
    }

    tracing::info!(id = %job.id, url = %job.url, attempt = attempt_no, "digesting");
    let client = match jobs::build_client(shared.request_timeout, &shared.user_agent) {
        Ok(client) => client,
        Err(error) => {
            let note = error.to_string();
            return Err(fail(
                &shared,
                &job,
                attempt_no,
                &note,
                jobs::JobError::Request(error),
            ));
        }
    };

    match jobs::digest(&client, &job.url).await {
        Ok(digested) => {
            tracing::info!(id = %job.id, sha256 = %digested.sha256, "digest complete");
            match shared
                .ledger
                .mark_done(&job.id, attempt_no, digested.title.as_deref(), &digested.sha256)
            {
                Ok(row) => (shared.events)(&row),
                Err(e) => tracing::warn!(id = %job.id, error = %e, "ledger mark_done failed"),
            }
            Ok(())
        }
        Err(error) => {
            let final_attempt = attempt_no >= MAX_ATTEMPTS;
            tracing::warn!(id = %job.id, attempt = attempt_no, %error, "digest failed");
            if let Ok(row) =
                shared
                    .ledger
                    .mark_error(&job.id, attempt_no, &error.to_string(), final_attempt)
            {
                (shared.events)(&row);
            }
            Err(error)
        }
    }
}

/// Record a hard failure and surface it to the retry layer.
fn fail(
    shared: &Shared,
    job: &DigestJob,
    attempt_no: u32,
    note: &str,
    error: jobs::JobError,
) -> jobs::JobError {
    let final_attempt = attempt_no >= MAX_ATTEMPTS;
    if let Ok(row) = shared.ledger.mark_error(&job.id, attempt_no, note, final_attempt) {
        (shared.events)(&row);
    }
    error
}

#[cfg(not(target_os = "android"))]
/// Desktop ingest: URL lines from stdin; EOF requests a graceful stop.
/// `lines_tx` is the feeding end for the supervised stdin-reader child.
pub fn link_ingest(
    lines_tx: mpsc::UnboundedSender<String>,
    mut lines: mpsc::UnboundedReceiver<String>,
) -> Box<dyn Service> {
    service("link-ingest", Kind::Frontline, move |ctx: Context| async move {
        // Blocking stdin reads run as a supervised child of this service.
        // The child owns the LAST sender: when stdin hits EOF the channel
        // closes and the loop below observes `None`.
        ctx.spawn("stdin-reader", {
            async move {
                use tokio::io::AsyncBufReadExt;
                let stdin = tokio::io::stdin();
                let mut reader = tokio::io::BufReader::new(stdin).lines();
                loop {
                    match reader.next_line().await {
                        Ok(Some(line)) => {
                            if lines_tx.send(line).is_err() {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(error = %e, "stdin read error");
                            break;
                        }
                    }
                }
                Ok(())
            }
        })
        .map_err(|e| Error::internal(format!("stdin spawn: {e:?}")))?;
        ctx.ready();
        loop {
            tokio::select! {
                biased;
                _ = ctx.stopped() => return Ok(()),
                line = lines.recv() => match line {
                    Some(line) => {
                        if let Some(shared) = shared_state() {
                            let _ = shared.enqueue_tx.send(line);
                        }
                    }
                    None => {
                        // stdin EOF: let the pipeline finish what it owes
                        // (every piped URL must reach a terminal state), then
                        // request the graceful stop.
                        if let Some(shared) = shared_state() {
                            wait_for_quiescence(&shared).await;
                        }
                        ctx.control().stop(rivium::StopReason::Requested);
                        return Ok(());
                    }
                },
            }
        }
    })
}
