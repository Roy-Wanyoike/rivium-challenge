package com.droidcon.digger

import android.content.Context
import android.database.sqlite.SQLiteDatabase
import java.io.File
import java.util.concurrent.Executors

/**
 * Startup hydration of the UI state from the SQLite mirror the Rust host
 * owns (`<filesDir>/data/jobs.db`): digests and the archived speaker list
 * reappear immediately after the app is reopened — before any new event —
 * because the rows were persisted by previous runs.
 */
object DigestStore {

    private val executor = Executors.newSingleThreadExecutor { r ->
        Thread(r, "digger-hydrate").apply { isDaemon = true }
    }

    /** Read both tables off the main thread; results merge additively. */
    fun hydrate(context: Context) {
        executor.execute {
            val db = File(File(context.filesDir, "data"), "jobs.db")
            JobEvents.hydrateJobs(loadJobs(db))
            JobEvents.upsertSpeakers(loadSpeakers(db))
        }
    }

    private inline fun <T> read(db: File, block: (SQLiteDatabase) -> T): T? =
        try {
            if (!db.exists()) {
                null
            } else {
                // The Rust host owns the schema and created the file; this
                // connection never writes a byte. READWRITE (no CREATE)
                // sidesteps the read-only/WAL open pitfalls.
                SQLiteDatabase.openDatabase(
                    db.absolutePath,
                    null,
                    SQLiteDatabase.OPEN_READWRITE,
                ).use { handle -> block(handle) }
            }
        } catch (_: Throwable) {
            // A missing/corrupt mirror must never take the UI down.
            null
        }

    private fun loadJobs(db: File): List<JobRow> =
        read(db) { handle ->
            val out = ArrayList<JobRow>()
            handle.rawQuery(
                "SELECT id, url, state, title, sha256, attempts, error, updated_at " +
                    "FROM jobs ORDER BY updated_at DESC LIMIT 100",
                null,
            ).use { c ->
                while (c.moveToNext()) {
                    out.add(
                        JobRow(
                            id = c.getString(0),
                            url = c.getString(1),
                            state = c.getString(2),
                            title = c.getString(3)?.takeIf { it.isNotEmpty() },
                            sha256 = c.getString(4)?.takeIf { it.isNotEmpty() },
                            attempts = c.getInt(5),
                            error = c.getString(6)?.takeIf { it.isNotEmpty() },
                            updatedAt = c.getLong(7),
                        )
                    )
                }
            }
            out
        } ?: emptyList()

    private fun loadSpeakers(db: File): List<Speaker> =
        read(db) { handle ->
            val out = ArrayList<Speaker>()
            handle.rawQuery(
                "SELECT name, role, url FROM speakers ORDER BY rowid",
                null,
            ).use { c ->
                while (c.moveToNext()) {
                    val name = c.getString(0)?.trim().orEmpty()
                    if (name.isNotEmpty()) {
                        out.add(
                            Speaker(
                                name = name,
                                role = c.getString(1)?.trim()?.takeIf { it.isNotEmpty() },
                                url = c.getString(2).orEmpty(),
                            )
                        )
                    }
                }
            }
            out
        } ?: emptyList()
}
