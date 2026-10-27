//! Rivium services: the digest worker, the enqueue pump, and (desktop) the
//! stdin link ingest.
//!
//! Wiring recap:
//! - `link-ingest` (desktop only) reads URL lines from stdin and pushes raw
//!   strings into the process-wide enqueue channel (Android pushes the same
//!   channel from JNI via `NativeBridge.nativeEnqueue`).
//! - `enqueue-pump` owns that channel's receiving end, admits URLs into the
//!   digest index (dedupe per URL), and pushes `DigestJob`s into the
//!   **apalis-sqlite** durable queue (`<root>/data/queue.db`).
//! - `digest-worker` runs the apalis worker over `SqliteStorage` until the
//!   rivium stop token fires. Pushed tasks survive restarts; in-flight tasks
//!   orphaned by a crash are re-enqueued by the backend after a heartbeat
//!   timeout, so the worker owns no replay logic of its own.
//!
//! The digest index (`<root>/data/jobs.db`) mirrors statuses for the UI; the
//! apalis queue table is the durable source of truth for owed work.

use crate::jobs::{self, DigestJob, Speaker};
use crate::ledger::{Admit, JobRow, Ledger};
use apalis::layers::retry::RetryPolicy;
use apalis::layers::WorkerBuilderExt;
use apalis_core::backend::ext::poll_strategy::{PollWith, StreamStrategy};
use apalis_core::backend::TaskSink;
use apalis_core::error::WorkerError;
use apalis_core::task::attempt::Attempt;
use apalis_core::task::builder::TaskBuilder;
use apalis_core::task::context::TaskContext;
use apalis_core::task::task_fn::task_fn;
use apalis_core::worker::builder::WorkerBuilder;
use apalis_core::worker::ext::parallelize::ParallelizeExt;
use apalis_sqlite::callback::HookCallbackListener;
use apalis_sqlite::SqliteStorage;
use rivium::{Context, Error, Kind, Service, service};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Total attempts per job: 1 initial + 2 retries.
pub const MAX_ATTEMPTS: u32 = 3;
const MAX_RETRIES: usize = (MAX_ATTEMPTS - 1) as usize;

/// Quiescence poll interval and cap for the desktop EOF stop. The cap only
/// guards against a wedged queue; pushed tasks are durable in apalis-sqlite
/// and re-run on the next boot, so a timeout never loses acknowledged work.
const QUIESCE_POLL: Duration = Duration::from_millis(100);
const QUIESCE_POLLS: u32 = 3_000; // 5 minutes

/// Callback invoked on every index transition; desktop prints, Android
/// forwards to `JobEvents.onJobUpdate` over JNI.
pub type EventSink = Arc<dyn Fn(&JobRow) + Send + Sync>;

/// Callback invoked when a digest harvests a speaker list; desktop prints,
/// Android forwards to `JobEvents.onSpeakers` over JNI.
pub type SpeakerSink = Arc<dyn Fn(&str, &[Speaker]) + Send + Sync>;

/// The worker-side backend: apalis-sqlite woken instantly by SQLite update
/// hooks (no polling latency between a tile tap and the fetch).
pub type WorkerBackend =
    PollWith<SqliteStorage<DigestJob>, StreamStrategy<HookCallbackListener>>;

/// Everything the services and the task handler need.
pub struct Shared {
    pub ledger: Arc<Ledger>,
    /// Raw URLs coming from stdin (desktop) or JNI (Android).
    pub enqueue_tx: mpsc::UnboundedSender<String>,
    /// Durable apalis-sqlite queue; Clone, so JNI callers and the pump share it.
    pub queue: SqliteStorage<DigestJob>,
    pub events: EventSink,
    pub speakers_event: SpeakerSink,
    pub concurrency: usize,
    pub request_timeout: Duration,
    pub user_agent: String,
}

/// Access the process state installed by `App::services` (the task handler
/// cannot capture anything through the apalis `task_fn`).
pub fn shared_state() -> Option<Arc<Shared>> {
    crate::app::state()
}

/// Persist a digest job into the apalis-sqlite queue. `max_attempts` bounds
/// the storage-level retry net (crash/orphan path); the in-process retry
/// layer handles the immediate retries.
async fn push_digest(
    shared: &Shared,
    row: &JobRow,
) -> Result<(), Box<dyn std::error::Error>> {
    let task = TaskBuilder::new(DigestJob {
        id: row.id.clone(),
        url: row.url.clone(),
    })
    .max_attempts(MAX_ATTEMPTS as usize)
    .build();
    // SqliteStorage is Clone (Arc'd pool inside); push through a clone so the
    // shared handle stays shared. NOTE: no idempotency_key — the index's
    // admit() dedupe already prevents duplicate pushes, and `!URL` requeue
    // MUST be able to insert a fresh task for a URL that already has a
    // terminal row in the Jobs table.
    shared.queue.clone().push_task(task).await?;
    Ok(())
}

/// Admission loop: raw URL strings in, durable apalis-sqlite tasks out.
pub fn enqueue_pump(
    shared: Arc<Shared>,
    mut enqueue_rx: mpsc::UnboundedReceiver<String>,
) -> Box<dyn Service> {
    service("enqueue-pump", Kind::Frontline, move |ctx: Context| async move {
        // Pushed tasks are durable; on boot the backend itself re-enqueues
        // anything a crashed process left in flight. Just log what is owed.
        match shared.ledger.pending() {
            Ok(pending) if !pending.is_empty() => {
                tracing::info!(count = pending.len(), "resuming pending jobs");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "index read failed at boot"),
        }
        ctx.ready();
        loop {
            tokio::select! {
                biased;
                _ = ctx.stopped() => {
                    // Graceful stop: drain whatever is still buffered (e.g. a
                    // last-millisecond JNI enqueue on Android) so it is
                    // persisted now and picked up on the next boot.
                    while let Ok(line) = enqueue_rx.try_recv() {
                        admit_line(&shared, &line).await;
                    }
                    return Ok(());
                }
                maybe_url = enqueue_rx.recv() => match maybe_url {
                    Some(line) => admit_line(&shared, &line).await,
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
        "queue did not drain before stop; pushed tasks are durable and will re-run on next boot"
    );
}

/// `URL` enqueues; `!URL` re-queues a previously failed job; `#...` comments.
async fn admit_line(shared: &Shared, line: &str) {
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
                if let Err(e) = push_digest(shared, &row).await {
                    tracing::error!(url, error = %e, "durable queue refused task");
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
                    if let Err(e) = push_digest(shared, &row).await {
                        tracing::error!(url, error = %e, "durable queue refused task");
                    }
                }
                Ok(None) => tracing::error!(url, "admitted row vanished"),
                Err(e) => tracing::error!(url, error = %e, "index read failed"),
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
        Err(e) => tracing::error!(url, error = %e, "index admit failed"),
    }
}

/// The apalis worker as a supervised rivium service.
pub fn digest_worker(
    shared: Arc<Shared>,
    backend: WorkerBackend,
) -> Box<dyn Service> {
    service("digest-worker", Kind::Frontline, move |ctx: Context| async move {
        ctx.ready();
        let concurrency = shared.concurrency.clamp(1, 16);
        let policy = RetryPolicy::retries(MAX_RETRIES);
        // Unique-per-process worker name: apalis-sqlite keys worker rows by
        // name, so a restarted process must not adopt the crashed run's row —
        // the stale row ages out instead and the live worker's heartbeat
        // sweep re-enqueues the tasks it left locked.
        let worker_name = format!("dig-worker-{}", std::process::id());
        let worker = WorkerBuilder::new(worker_name.as_str())
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
            // The speakers directory feeds its own read model: swap the
            // harvested list into the ledger, then hand it to the UI.
            if !digested.speakers.is_empty() {
                match shared.ledger.replace_speakers(&job.url, &digested.speakers) {
                    Ok(stored) => {
                        tracing::info!(
                            id = %job.id,
                            speakers = stored,
                            "speaker list archived"
                        );
                        (shared.speakers_event)(&job.url, &digested.speakers);
                    }
                    Err(e) => {
                        tracing::warn!(id = %job.id, error = %e, "ledger replace_speakers failed")
                    }
                }
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

/// The apalis-board queue inspector as a supervised rivium service.
///
/// Desktop-only and opt-in on two axes (the challenge's "keep it small"):
/// the cargo `board` feature pulls the dependency, and a non-empty
/// `board_addr` config (`--set board_addr=127.0.0.1:8080`) starts the
/// service. The UI registers the SAME `SqliteStorage` clone the pump pushes
/// through, so the dashboard reflects the durable queue itself — not a
/// copy. Android never compiles this.
#[cfg(all(feature = "board", not(target_os = "android")))]
pub fn board_server(queue: SqliteStorage<DigestJob>, addr: std::net::SocketAddr) -> Box<dyn Service> {
    service("board", Kind::Frontline, move |ctx: Context| async move {
        // SSE hub for the UI's /logs page; rivium owns the global tracing
        // subscriber, so live log streaming stays empty unless the host is
        // re-launched with the board's subscriber layer composed in.
        let broadcaster = apalis_board::axum::sse::TracingBroadcaster::create();
        use apalis_board::axum::framework::{ApiBuilder, RegisterRoute};
        let api = ApiBuilder::new(axum::Router::new())
            .register("digest", queue)
            .build();
        let router = axum::Router::new()
            .nest("/api/v1", api)
            .fallback_service(apalis_board::axum::ui::ServeUI::new())
            .layer(axum::Extension(broadcaster));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| Error::internal(format!("board bind {addr}: {e}")))?;
        tracing::info!("apalis-board UI on http://{addr}");
        ctx.ready();
        axum::serve(listener, router)
            .with_graceful_shutdown(async move { ctx.stopped().await })
            .await
            .map_err(|e| Error::internal(format!("board serve: {e}")))?;
        Ok(())
    })
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
