//! `digd` — the desktop digger daemon.
//!
//! Usage:
//! ```text
//! digd --root ./demo-root
//! # then paste URLs on stdin, one per line, e.g.:
//! #   https://droidcon.co.ke
//! #   !https://droidcon.co.ke/sponsors   (re-queue a failed job)
//! # EOF (Ctrl-D) stops the host gracefully.
//! ```

fn main() -> rivium_host::Code {
    rivium_host::process::run::<session_digger::app::DiggerApp>()
}
