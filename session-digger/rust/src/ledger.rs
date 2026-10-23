//! SQLite job ledger: the single source of truth for digest work.
//!
//! The in-process task channel (see [`crate::queue`]) is only a transport;
//! every durable state transition goes through this module so a SIGKILL at
//! any point replays cleanly on the next boot.

use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest as _, Sha256};
use std::path::Path;
use std::sync::Mutex;

/// Lifecycle of a digest job, persisted verbatim in the `state` column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Failed => "failed",
        }
    }

    fn from_str(raw: &str) -> Self {
        match raw {
            "running" => JobState::Running,
            "done" => JobState::Done,
            "failed" => JobState::Failed,
            _ => JobState::Queued,
        }
    }
}

/// One ledger row as observed by callers and event sinks.
#[derive(Clone, Debug, serde::Serialize)]
pub struct JobRow {
    pub id: String,
    pub url: String,
    pub state: String,
    pub title: Option<String>,
    pub sha256: Option<String>,
    pub attempts: u32,
    pub error: Option<String>,
    pub updated_at: i64,
}

/// Deterministic job id derived from the URL: re-submitting the same link
/// after a restart can never duplicate work.
pub fn job_id_for(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    let digest = hasher.finalize();
    digest[..12].iter().map(|b| format!("{b:02x}")).collect()
}

/// Process-wide outcome of offering a URL to the ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admit {
    /// Freshly inserted as `queued`; the caller must enqueue a task.
    Admitted,
    /// Already tracked and not in a terminal state; replayed at boot instead.
    AlreadyPending,
    /// Already digested successfully; nothing to do.
    AlreadyDone,
    /// Previously failed permanently; retried on explicit request only.
    AlreadyFailed,
}

pub struct Ledger(Mutex<Connection>);

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS jobs (
    id         TEXT PRIMARY KEY,
    url        TEXT NOT NULL,
    state      TEXT NOT NULL DEFAULT 'queued',
    title      TEXT,
    sha256     TEXT,
    attempts   INTEGER NOT NULL DEFAULT 0,
    error      TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS jobs_state ON jobs(state);
";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Ledger {
    /// Open (creating if needed) the ledger at `<root>/jobs.db`.
    pub fn open(root: &Path) -> rusqlite::Result<Self> {
        std::fs::create_dir_all(root)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let conn = Connection::open(root.join("jobs.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self(Mutex::new(conn)))
    }

    fn row_from(stmt_row: &rusqlite::Row<'_>) -> rusqlite::Result<JobRow> {
        Ok(JobRow {
            id: stmt_row.get(0)?,
            url: stmt_row.get(1)?,
            state: stmt_row.get(2)?,
            title: stmt_row.get(3)?,
            sha256: stmt_row.get(4)?,
            attempts: stmt_row.get::<_, i64>(5)? as u32,
            error: stmt_row.get(6)?,
            updated_at: stmt_row.get(7)?,
        })
    }

    const COLUMNS: &'static str =
        "id, url, state, title, sha256, attempts, error, updated_at";

    /// Offer a URL for digestion. Idempotent per URL.
    pub fn admit(&self, url: &str) -> rusqlite::Result<Admit> {
        let id = job_id_for(url);
        let conn = self.0.lock().expect("ledger poisoned");
        let existing: Option<String> = conn
            .query_row("SELECT state FROM jobs WHERE id = ?1", [&id], |r| r.get(0))
            .optional()?;
        if let Some(state) = existing {
            return Ok(match JobState::from_str(&state) {
                JobState::Queued | JobState::Running => Admit::AlreadyPending,
                JobState::Done => Admit::AlreadyDone,
                JobState::Failed => Admit::AlreadyFailed,
            });
        }
        let ts = now();
        conn.execute(
            "INSERT INTO jobs (id, url, state, created_at, updated_at) \
             VALUES (?1, ?2, 'queued', ?3, ?3)",
            rusqlite::params![id, url, ts],
        )?;
        Ok(Admit::Admitted)
    }

    /// Force a previously failed URL back into the queue.
    pub fn requeue_failed(&self, url: &str) -> rusqlite::Result<Option<JobRow>> {
        let id = job_id_for(url);
        let conn = self.0.lock().expect("ledger poisoned");
        let changed = conn.execute(
            "UPDATE jobs SET state = 'queued', error = NULL, updated_at = ?2 \
             WHERE id = ?1 AND state = 'failed'",
            rusqlite::params![id, now()],
        )?;
        drop(conn);
        if changed == 0 {
            return Ok(None);
        }
        self.get(&id)
    }

    fn update_state(
        &self,
        id: &str,
        state: JobState,
        attempts: u32,
        title: Option<&str>,
        sha256: Option<&str>,
        error: Option<&str>,
    ) -> rusqlite::Result<JobRow> {
        let conn = self.0.lock().expect("ledger poisoned");
        conn.execute(
            "UPDATE jobs SET state = ?2, attempts = ?3, title = ?4, sha256 = ?5, \
             error = ?6, updated_at = ?7 WHERE id = ?1",
            rusqlite::params![
                id,
                state.as_str(),
                attempts as i64,
                title,
                sha256,
                error,
                now()
            ],
        )?;
        drop(conn);
        self.get(id)?.ok_or_else(|| {
            rusqlite::Error::QueryReturnedNoRows
        })
    }

    /// Transition into `running` for the given 1-based attempt number.
    pub fn mark_running(&self, id: &str, attempt: u32) -> rusqlite::Result<JobRow> {
        self.update_state(id, JobState::Running, attempt, None, None, None)
    }

    /// Record a successful digest.
    pub fn mark_done(
        &self,
        id: &str,
        attempts: u32,
        title: Option<&str>,
        sha256: &str,
    ) -> rusqlite::Result<JobRow> {
        self.update_state(id, JobState::Done, attempts, title, Some(sha256), None)
    }

    /// Record a failure; `final` flips the row to `failed`, otherwise the job
    /// returns to `queued` so the retry layer's next attempt can pick it up.
    pub fn mark_error(
        &self,
        id: &str,
        attempts: u32,
        error: &str,
        final_attempt: bool,
    ) -> rusqlite::Result<JobRow> {
        // Truncate on char boundaries: URLs and error text may be multi-byte.
        let mut note = error.to_string();
        if note.chars().count() > 300 {
            note = note.chars().take(300).collect();
        }
        let state = if final_attempt {
            JobState::Failed
        } else {
            JobState::Queued
        };
        self.update_state(id, state, attempts, None, None, Some(&note))
    }

    /// Fetch one row by id.
    pub fn get(&self, id: &str) -> rusqlite::Result<Option<JobRow>> {
        let conn = self.0.lock().expect("ledger poisoned");
        conn.query_row(
            &format!("SELECT {} FROM jobs WHERE id = ?1", Self::COLUMNS),
            [id],
            Self::row_from,
        )
        .optional()
    }

    /// All rows still owed work: `queued` plus any `running` survivor of a crash.
    pub fn pending(&self) -> rusqlite::Result<Vec<JobRow>> {
        let conn = self.0.lock().expect("ledger poisoned");
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM jobs WHERE state IN ('queued', 'running') \
             ORDER BY created_at, id",
            Self::COLUMNS
        ))?;
        let rows = stmt
            .query_map([], Self::row_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Newest rows for status display.
    pub fn recent(&self, limit: u32) -> rusqlite::Result<Vec<JobRow>> {
        let conn = self.0.lock().expect("ledger poisoned");
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM jobs ORDER BY updated_at DESC, id LIMIT ?1",
            Self::COLUMNS
        ))?;
        let rows = stmt
            .query_map([limit as i64], Self::row_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Reset crash-orphaned `running` rows back to `queued`; returns count.
    pub fn reset_running(&self) -> rusqlite::Result<usize> {
        let conn = self.0.lock().expect("ledger poisoned");
        conn.execute(
            "UPDATE jobs SET state = 'queued', updated_at = ?1 WHERE state = 'running'",
            [now()],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_ledger() -> (tempfile::TempDir, Ledger) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(dir.path()).unwrap();
        (dir, ledger)
    }

    #[test]
    fn admit_is_idempotent_per_url() {
        let (_dir, ledger) = temp_ledger();
        assert_eq!(ledger.admit("https://droidcon.co.ke").unwrap(), Admit::Admitted);
        assert_eq!(
            ledger.admit("https://droidcon.co.ke").unwrap(),
            Admit::AlreadyPending
        );
        let id = job_id_for("https://droidcon.co.ke");
        let row = ledger.mark_running(&id, 1).unwrap();
        assert_eq!(row.state, "running");
        let row = ledger.mark_done(&id, 1, Some("droidcon Kenya"), "abc").unwrap();
        assert_eq!(row.state, "done");
        assert_eq!(
            ledger.admit("https://droidcon.co.ke").unwrap(),
            Admit::AlreadyDone
        );
    }

    #[test]
    fn crash_orphans_replay() {
        let (_dir, ledger) = temp_ledger();
        ledger.admit("https://droidcon.co.ke/speakers").unwrap();
        let id = job_id_for("https://droidcon.co.ke/speakers");
        ledger.mark_running(&id, 1).unwrap();
        // Simulated SIGKILL: nothing marks done; next boot resets and replays.
        assert_eq!(ledger.reset_running().unwrap(), 1);
        let pending = ledger.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].state, "queued");
    }

    #[test]
    fn error_truncation_is_char_safe() {
        let (_dir, ledger) = temp_ledger();
        let url = "https://example.com/naïve-🦀-path";
        ledger.admit(url).unwrap();
        let id = job_id_for(url);
        let junk = "🦀".repeat(400);
        let row = ledger.mark_error(&id, 3, &junk, true).unwrap();
        assert_eq!(row.state, "failed");
        assert!(row.error.as_ref().unwrap().chars().count() <= 300);
    }
}
