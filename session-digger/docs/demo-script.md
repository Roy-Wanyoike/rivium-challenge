# 90-second demo script (screen recording)

One continuous take, phone in hand. Total ≈ 90s. Record at 1080p+.

| Time | Action | Say / show |
| ---- | ------ | ---------- |
| 0:00–0:08 | Home screen, swipe down to Quick Settings | "This is session-digger — a droidcon Kenya link digger built on the Rivium runtime." |
| 0:08–0:15 | Tap the **Digger** QS tile | "One tap enqueues the droidcon homepage straight into the Rust queue — no app in the foreground." Tile subtitle shows `1 in queue`. |
| 0:15–0:25 | Open the app | "The Compose UI mirrors the SQLite ledger live — every state change is a JNI callback from Rust." Card shows `QUEUED → RUNNING`. |
| 0:25–0:40 | Point at the DONE card | "Done. We fetched the page, extracted the title, and hashed the body — SHA-256: b2cd120f…7638e. The ledger is the source of truth." |
| 0:40–0:55 | Show airplane mode, enqueue another URL, wait for FAILED | "No network? The apalis worker retries twice, then parks the job as failed — with the error stored." |
| 0:55–1:10 | Kill the app mid-flight: recents → swipe away, reopen | "Here's the part I care about: kill it mid-fetch…" Ledger shows the orphaned job resuming to DONE. "…the enqueue pump replays unfinished jobs from SQLite on boot. SIGKILL-proof." |
| 1:10–1:25 | (Cut to terminal, optional) `digd --root demo-root` desktop run, pipe a URL, `Ctrl-D` | "Same core runs as a desktop daemon through the same rivium host — one codebase, two runtimes." |
| 1:25–1:30 | End on the DONE card / repo README | "Rivium services, custom apalis backend, JNI bridge — code in the repo." |

## Recording checklist

- [ ] Device clock visible? (not required, but helps review)
- [ ] Do Not Disturb OFF so the "digger: done" notification drops visibly in step 6
- [ ] Pre-enqueue one job before recording so the tile subtitle shows a queue count naturally
- [ ] Desktop fallback clip: `echo "https://droidcon.co.ke" | ./target/debug/digd --root ./demo-root`
