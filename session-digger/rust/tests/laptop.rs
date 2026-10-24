//! Laptop end-to-end tests: real `digd` process, real network, real SQLite.
//!
//! These need outbound HTTP and are skipped by default:
//! ```text
//! cargo test --test laptop -- --ignored --nocapture
//! ```

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const URL_HOME: &str = "https://droidcon.co.ke";
const POLL: Duration = Duration::from_millis(200);
const BUDGET: Duration = Duration::from_secs(90);

struct Daemon {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
}

impl Daemon {
    fn spawn(root: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_digd"))
            .arg("--root")
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn digd");
        let stdin = child.stdin.take().expect("digd stdin");
        Self {
            child,
            stdin: Some(stdin),
        }
    }

    fn submit(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("digd stdin still open");
        writeln!(stdin, "{line}").expect("write URL to digd stdin");
        stdin.flush().expect("flush digd stdin");
    }

    fn kill9(&mut self) {
        #[cfg(unix)]
        {
            // SIGKILL: no graceful drain, exactly the crash-replay scenario.
            unsafe {
                libc_kill(self.child.id() as i32, 9);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ledger_path(root: &Path) -> PathBuf {
    root.join("data").join("jobs.db")
}

fn query(root: &Path, sql: &str) -> Vec<(String, String, i64)> {
    let conn = match rusqlite::Connection::open_with_flags(
        ledger_path(root),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(conn) => conn,
        // The daemon creates the ledger lazily; treat absence as empty.
        Err(_) => return Vec::new(),
    };
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, i64>(2)?,
            ))
        })
        .map(|mapped| mapped.map(Result::unwrap).collect())
        .unwrap_or_default();
    drop(stmt);
    rows
}

fn wait_for(root: &Path, sql: &str, want: &str) -> Vec<(String, String, i64)> {
    let deadline = Instant::now() + BUDGET;
    loop {
        assert!(deadline > Instant::now(), "timed out waiting for {want}");
        let rows = query(root, sql);
        if !rows.is_empty() {
            return rows;
        }
        std::thread::sleep(POLL);
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn temp_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "digd-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp root");
    dir
}

/// Full happy path: enqueue via stdin, watch the ledger reach `done`,
/// graceful stop on EOF.
#[test]
#[ignore = "requires network"]
fn e2e_digest_flow() {
    let root = temp_root("flow");
    let mut digd = Daemon::spawn(&root);
    digd.submit(URL_HOME);

    let rows = wait_for(
        &root,
        &format!("SELECT url, sha256, attempts FROM jobs WHERE url = '{URL_HOME}' AND state = 'done'"),
        "done row",
    );
    let (_, sha256, attempts) = &rows[0];
    assert!(is_sha256(sha256), "sha256 must be 64 hex chars: {sha256}");
    assert_eq!(*attempts, 1, "first attempt should succeed");

    // EOF stops the host; exit code 0 means a clean drain.
    drop(digd.stdin.take());
    let status = digd.child.wait().expect("wait for digd exit");
    assert!(status.success(), "graceful EOF stop must exit 0");

    // The ledger keeps exactly one row per URL.
    let all = query(&root, "SELECT url, state, attempts FROM jobs");
    assert_eq!(all.len(), 1);
}

/// Crash-replay: SIGKILL mid-flight, restart on the same root, and the
/// orphaned job finishes with a stable hash.
#[test]
#[ignore = "requires network"]
#[cfg(unix)]
fn e2e_sigkill_replay() {
    let root = temp_root("kill");
    let mut digd = Daemon::spawn(&root);
    digd.submit(URL_HOME);

    // Wait until the fetch is in flight, then shoot it down.
    let deadline = Instant::now() + BUDGET;
    loop {
        assert!(deadline > Instant::now(), "never reached running");
        let rows = query(
            &root,
            &format!("SELECT url, state, attempts FROM jobs WHERE url = '{URL_HOME}'"),
        );
        if rows.iter().any(|(_, state, _)| state == "running") {
            break;
        }
        std::thread::sleep(POLL);
    }
    digd.kill9();

    let mid = query(&root, "SELECT url, state, attempts FROM jobs");
    assert!(
        mid.iter().any(|(_, s, _)| s == "running" || s == "queued"),
        "expected an unfinished row after SIGKILL, got {mid:?}"
    );

    // Fresh process, same ledger: the pump must replay and finish the job.
    let _digd = Daemon::spawn(&root);
    let rows = wait_for(
        &root,
        &format!("SELECT url, sha256, attempts FROM jobs WHERE url = '{URL_HOME}' AND state = 'done'"),
        "replayed done row",
    );
    assert!(is_sha256(&rows[0].1));
    // Replay must not duplicate rows.
    let all = query(&root, "SELECT url, state, attempts FROM jobs");
    assert_eq!(all.len(), 1, "no duplicate rows after replay");
}

/// Failure path: a 404 URL is retried `MAX_ATTEMPTS` times, then parks in
/// `failed`; a later `!URL` line re-queues it for a fresh round.
#[test]
#[ignore = "requires network"]
fn e2e_failure_then_requeue() {
    let root = temp_root("fail");
    let mut digd = Daemon::spawn(&root);
    digd.submit("https://droidcon.co.ke/no-such-page-xyz");

    let rows = wait_for(
        &root,
        "SELECT url, state, attempts FROM jobs WHERE state = 'failed'",
        "failed row",
    );
    assert_eq!(rows[0].2, 3, "must exhaust 1 initial + 2 retries");

    digd.submit("!https://droidcon.co.ke/no-such-page-xyz");
    let deadline = Instant::now() + BUDGET;
    loop {
        assert!(deadline > Instant::now(), "requeue never ran again");
        let rows = query(
            &root,
            "SELECT url, state, attempts FROM jobs WHERE state = 'running' OR attempts > 3",
        );
        if !rows.is_empty() {
            break;
        }
        std::thread::sleep(POLL);
    }
}
