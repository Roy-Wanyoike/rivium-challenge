//! SQLite digest index: one row per digested URL, mirroring apalis task
//! statuses as they change.
//!
//! The **durable queue** is `apalis-sqlite` (`<root>/data/queue.db`): pushed
//! tasks survive restarts, and orphaned in-flight tasks are re-enqueued by
//! the backend itself after a heartbeat timeout. This module is the
//! lightweight **results index** the UI and the QS tile read: title, SHA-256,
//! error, attempt count — using apalis-core's own [`Status`] vocabulary so
//! both views of a job agree.

use crate::jobs::Speaker;
use apalis_core::task::status::Status;
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest as _, Sha256};
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;

/// Parse a persisted status, tolerating legacy lowercase spellings.
fn parse_status(raw: &str) -> Status {
    Status::from_str(raw).unwrap_or_else(|_| match raw.to_ascii_lowercase().as_str() {
        "running" => Status::Running,
        "done" => Status::Done,
        "failed" => Status::Failed,
        "killed" => Status::Killed,
        _ => Status::Queued,
    })
}

/// Whether the job still owes work (used for quiescence and tile counts).
fn is_pending(status: &Status) -> bool {
    matches!(status, Status::Pending | Status::Queued | Status::Running)
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

/// One stored speaker, as read by the UI (insertion order preserved).
#[derive(Clone, Debug, serde::Serialize)]
pub struct SpeakerRow {
    pub name: String,
    pub role: Option<String>,
    pub url: String,
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
CREATE TABLE IF NOT EXISTS speakers (
    url        TEXT NOT NULL,
    name       TEXT NOT NULL,
    role       TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (url, name)
);
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
            return Ok(match parse_status(&state) {
                s if is_pending(&s) => Admit::AlreadyPending,
                Status::Done => Admit::AlreadyDone,
                _ => Admit::AlreadyFailed,
            });
        }
        let ts = now();
        conn.execute(
            "INSERT INTO jobs (id, url, state, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?4)",
            rusqlite::params![id, url, Status::Queued.to_string(), ts],
        )?;
        Ok(Admit::Admitted)
    }

    /// Force a previously failed URL back into the queue.
    pub fn requeue_failed(&self, url: &str) -> rusqlite::Result<Option<JobRow>> {
        let id = job_id_for(url);
        let conn = self.0.lock().expect("ledger poisoned");
        let changed = conn.execute(
            "UPDATE jobs SET state = ?2, error = NULL, updated_at = ?3 \
             WHERE id = ?1 AND state = ?4",
            rusqlite::params![
                id,
                Status::Queued.to_string(),
                now(),
                Status::Failed.to_string()
            ],
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
        state: Status,
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
                state.to_string(),
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

    /// Transition into `Running` for the given 1-based attempt number.
    pub fn mark_running(&self, id: &str, attempt: u32) -> rusqlite::Result<JobRow> {
        self.update_state(id, Status::Running, attempt, None, None, None)
    }

    /// Record a successful digest.
    pub fn mark_done(
        &self,
        id: &str,
        attempts: u32,
        title: Option<&str>,
        sha256: &str,
    ) -> rusqlite::Result<JobRow> {
        self.update_state(id, Status::Done, attempts, title, Some(sha256), None)
    }

    /// Record a failure; `final` parks the row in `Failed`, otherwise the job
    /// returns to `Queued` so the retry layer's next attempt can pick it up.
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
            Status::Failed
        } else {
            Status::Queued
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

    /// All rows still owed work: `Queued` plus any `Running` survivor of a
    /// crash (the apalis-sqlite backend re-enqueues the latter itself).
    pub fn pending(&self) -> rusqlite::Result<Vec<JobRow>> {
        let conn = self.0.lock().expect("ledger poisoned");
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM jobs ORDER BY created_at, id",
            Self::COLUMNS
        ))?;
        let rows = stmt
            .query_map([], Self::row_from)?
            .filter_map(|row| {
                let row = row.ok()?;
                let status = parse_status(&row.state);
                is_pending(&status).then_some(row)
            })
            .collect::<Vec<_>>();
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

    /// Transactionally replace the speaker list harvested from `url`.
    /// Re-digesting the directory swaps the stored list in one go; names are
    /// unique per URL (the pair is the primary key). Returns rows stored.
    pub fn replace_speakers(&self, url: &str, speakers: &[Speaker]) -> rusqlite::Result<usize> {
        let conn = self.0.lock().expect("ledger poisoned");
        // Explicit transaction: an outside reader (the UI hydration
        // connection) never sees the delete without the re-insert.
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM speakers WHERE url = ?1", [url])?;
        let mut stored = 0usize;
        for speaker in speakers {
            stored += tx.execute(
                "INSERT OR IGNORE INTO speakers (url, name, role, updated_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![url, speaker.name, speaker.role, now()],
            )?;
        }
        tx.commit()?;
        Ok(stored)
    }

    /// Every stored speaker, in harvest order (the UI's read model).
    pub fn speakers(&self) -> rusqlite::Result<Vec<SpeakerRow>> {
        let conn = self.0.lock().expect("ledger poisoned");
        let mut stmt = conn.prepare(
            "SELECT name, role, url, updated_at FROM speakers ORDER BY rowid",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SpeakerRow {
                    name: r.get(0)?,
                    role: r.get(1)?,
                    url: r.get(2)?,
                    updated_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
        assert_eq!(row.state, "Running");
        let row = ledger.mark_done(&id, 1, Some("droidcon Kenya"), "abc").unwrap();
        assert_eq!(row.state, "Done");
        assert_eq!(
            ledger.admit("https://droidcon.co.ke").unwrap(),
            Admit::AlreadyDone
        );
    }

    #[test]
    fn crash_orphans_stay_pending_for_the_backend() {
        let (_dir, ledger) = temp_ledger();
        ledger.admit("https://droidcon.co.ke/speakers").unwrap();
        let id = job_id_for("https://droidcon.co.ke/speakers");
        ledger.mark_running(&id, 1).unwrap();
        // Simulated SIGKILL: the index row stays `Running`; the apalis-sqlite
        // backend owns re-enqueueing the orphaned task after its heartbeat
        // timeout. Quiescence still sees it as pending either way.
        let pending = ledger.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].state, "Running");
    }

    #[test]
    fn legacy_lowercase_rows_still_parse() {
        let (_dir, ledger) = temp_ledger();
        ledger.admit("https://example.com/legacy").unwrap();
        let id = job_id_for("https://example.com/legacy");
        {
            let conn = ledger.0.lock().unwrap();
            conn.execute(
                "UPDATE jobs SET state = 'done' WHERE id = ?1",
                [&id],
            )
            .unwrap();
        }
        assert_eq!(ledger.admit("https://example.com/legacy").unwrap(), Admit::AlreadyDone);
    }

    #[test]
    fn error_truncation_is_char_safe() {
        let (_dir, ledger) = temp_ledger();
        let url = "https://example.com/naïve-🦀-path";
        ledger.admit(url).unwrap();
        let id = job_id_for(url);
        let junk = "🦀".repeat(400);
        let row = ledger.mark_error(&id, 3, &junk, true).unwrap();
        assert_eq!(row.state, "Failed");
        assert!(row.error.as_ref().unwrap().chars().count() <= 300);
    }

    #[test]
    fn speakers_table_swaps_and_dedupes_per_url() {
        let (_dir, ledger) = temp_ledger();
        let url = "https://droidcon.co.ke/speakers";
        let first = vec![
            Speaker {
                name: "Harny Otuoniyo".into(),
                role: Some("Android Engineer at getyourguide".into()),
            },
            Speaker {
                name: "Dinoy Raj".into(),
                role: None,
            },
        ];
        assert_eq!(ledger.replace_speakers(url, &first).unwrap(), 2);
        // Same URL again: a full swap, no duplicates even with repeated names.
        let second = vec![
            Speaker {
                name: "Harny Otuoniyo".into(),
                role: Some("new role".into()),
            },
            Speaker {
                name: "Harny Otuoniyo".into(),
                role: None,
            },
            Speaker {
                name: "New Speaker".into(),
                role: None,
            },
        ];
        assert_eq!(ledger.replace_speakers(url, &second).unwrap(), 2);
        let rows = ledger.speakers().unwrap();
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["Harny Otuoniyo", "New Speaker"]
        );
        assert_eq!(rows[0].role.as_deref(), Some("new role"));
        assert_eq!(rows[0].url, url);
        // A different URL keeps its own list.
        assert_eq!(
            ledger
                .replace_speakers("https://droidcon.co.ke/2024/speakers", &first)
                .unwrap(),
            2
        );
        assert_eq!(ledger.speakers().unwrap().len(), 4);
    }
}
