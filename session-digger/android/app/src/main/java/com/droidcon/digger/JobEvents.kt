package com.droidcon.digger

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import org.json.JSONObject

/** One ledger row as pushed by the Rust event sink. */
data class JobRow(
    val id: String,
    val url: String,
    val state: String,
    val title: String? = null,
    val sha256: String? = null,
    val attempts: Int = 0,
    val error: String? = null,
    val updatedAt: Long = 0L,
)

/** One speaker harvested from the droidcon speakers page. */
data class Speaker(
    val name: String,
    val role: String? = null,
    val url: String = "",
)

/**
 * Rust → Java callback surface and in-app state mirror.
 *
 * `onJobUpdate` is invoked from Rust worker threads (see
 * rust/src/bridge.rs); it must stay a cheap static call — it upserts one row
 * into a StateFlow and posts a notification for terminal states.
 */
object JobEvents {

    private const val TAG = "JobEvents"
    private const val CHANNEL_ID = "digest-results"

    private val _jobs = MutableStateFlow<List<JobRow>>(emptyList())
    val jobs: StateFlow<List<JobRow>> = _jobs

    /** Archived speakers (droidcon.co.ke/speakers), harvest order kept. */
    private val _speakers = MutableStateFlow<List<Speaker>>(emptyList())
    val speakers: StateFlow<List<Speaker>> = _speakers

    /** Newest-first read model for Compose and the QS tile. */
    val rows: List<JobRow> get() = _jobs.value

    fun pendingCount(): Int = _jobs.value.count { it.state == "Queued" || it.state == "Running" }

    /** Called by Rust. Must not throw — panics unwind into FFI wrappers. */
    @JvmStatic
    fun onJobUpdate(jobJson: String) {
        try {
            val obj = JSONObject(jobJson)
            val row = JobRow(
                id = obj.optString("id"),
                url = obj.optString("url"),
                state = obj.optString("state"),
                title = obj.optString("title").takeIf { it.isNotEmpty() },
                sha256 = obj.optString("sha256").takeIf { it.isNotEmpty() },
                attempts = obj.optInt("attempts", 0),
                error = obj.optString("error").takeIf { it.isNotEmpty() },
                updatedAt = obj.optLong("updated_at", 0L),
            )
            if (row.id.isEmpty()) return
            synchronized(this) {
                val rest = _jobs.value.filterNot { it.id == row.id }
                _jobs.value = (listOf(row) + rest).sortedByDescending { it.updatedAt }
            }
            if (row.state == "Done" || row.state == "Failed") notify(row)
        } catch (t: Throwable) {
            // Never propagate into FFI.
            Log.w(TAG, "bad event payload", t)
        }
    }

    /**
     * Called by Rust after the speakers page is digested. Payload:
     * `{"url": …, "speakers": [{"name": …, "role": …}, …]}`. Must not throw.
     */
    @JvmStatic
    fun onSpeakers(payloadJson: String) {
        try {
            val obj = JSONObject(payloadJson)
            val url = obj.optString("url")
            val arr = obj.optJSONArray("speakers") ?: return
            val list = ArrayList<Speaker>(arr.length())
            for (i in 0 until arr.length()) {
                val entry = arr.optJSONObject(i) ?: continue
                val name = entry.optString("name").trim()
                if (name.isEmpty()) continue
                list.add(
                    Speaker(
                        name = name,
                        role = entry.optString("role").takeIf { it.isNotEmpty() },
                        url = url,
                    )
                )
            }
            upsertSpeakers(list)
        } catch (t: Throwable) {
            // Never propagate into FFI.
            Log.w(TAG, "bad speakers payload", t)
        }
    }

    /** Merge speakers (live events or the SQLite mirror), keyed by name. */
    fun upsertSpeakers(incoming: List<Speaker>) {
        if (incoming.isEmpty()) return
        synchronized(this) {
            val merged = _speakers.value.filterNot { known ->
                incoming.any { it.name == known.name }
            }
            _speakers.value = merged + incoming
        }
    }

    /** Seed the job list from the SQLite mirror; additive, never clobbers. */
    fun hydrateJobs(rows: List<JobRow>) {
        if (rows.isEmpty()) return
        synchronized(this) {
            val known = _jobs.value.map { it.id }.toHashSet()
            val additions = rows.filter { it.id !in known }
            if (additions.isEmpty()) return
            _jobs.value = (additions + _jobs.value).sortedByDescending { it.updatedAt }
        }
    }

    /** Terminal-state notification. Safe to call before permission grant. */
    private fun notify(row: JobRow) {
        val context = AppContextHolder.get() ?: return
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                "Digest results",
                NotificationManager.IMPORTANCE_DEFAULT,
            )
        )
        if (ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS)
            != PackageManager.PERMISSION_GRANTED
        ) {
            return
        }
        val open = PendingIntent.getActivity(
            context,
            0,
            Intent(context, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val text = when (row.state) {
            "Done" -> row.title?.let { "$it — ${row.sha256?.take(12)}…" } ?: row.sha256.orEmpty()
            else -> row.error ?: "digest failed"
        }
        val notification = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(android.R.drawable.stat_sys_download_done)
            .setContentTitle("digger: ${row.state}")
            .setContentText(text)
            .setContentIntent(open)
            .setAutoCancel(true)
            .build()
        manager.notify(row.id.hashCode(), notification)
    }
}

/** Process-wide context handle for callbacks that arrive without one. */
object AppContextHolder {
    @Volatile private var appContext: Context? = null

    fun install(context: Context) {
        appContext = context.applicationContext
    }

    fun get(): Context? = appContext
}
