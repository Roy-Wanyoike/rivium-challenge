package com.droidcon.digger

import android.content.Context
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow

/**
 * Kotlin lifecycle wrapper around the Rust embedded host.
 *
 * `nativeStart` blocks until the supervisor reaches Running (or fails), so it
 * must never run on the UI thread — the single-thread executor here is the
 * host thread for the whole app lifetime.
 */
object RustHost {

    /** Mirrors rivium_host::session::Status discriminants. */
    const val STATUS_IDLE: Int = 0
    const val STATUS_STARTING: Int = 1
    const val STATUS_RUNNING: Int = 2
    const val STATUS_STOPPING: Int = 3
    const val STATUS_RESTARTING: Int = 4
    const val STATUS_FAILED: Int = 5

    private val executor = Executors.newSingleThreadExecutor { r ->
        Thread(r, "digger-rust-host").apply { isDaemon = true }
    }
    private val started = AtomicBoolean(false)

    private val _status = MutableStateFlow(STATUS_IDLE)
    val status: StateFlow<Int> = _status

    /** Start the host once; later calls are no-ops. Safe from any thread. */
    fun start(context: Context) {
        if (!started.compareAndSet(false, true)) return
        executor.execute {
            _status.value = STATUS_STARTING
            // The ledger lives under filesDir/data; survives reinstall cycles
            // of the demo without special storage permissions.
            val root = context.filesDir.absolutePath
            val code = NativeBridge.nativeStart(arrayOf("--root", root))
            _status.value = if (code == 0) STATUS_RUNNING else STATUS_FAILED
        }
    }

    /** Graceful stop with a 5s drain budget. */
    fun stop() {
        if (!started.compareAndSet(true, false)) return
        executor.execute {
            _status.value = STATUS_STOPPING
            NativeBridge.nativeStop(5_000L)
            _status.value = STATUS_IDLE
        }
    }

    /** Hand a URL to the Rust enqueue pump; returns the JNI result code. */
    fun enqueue(url: String): Int = NativeBridge.nativeEnqueue(url)

    /** Current host status snapshot. */
    fun statusNow(): Int = NativeBridge.nativeStatus()

    /** Last safe error summary from the Rust side, if any. */
    fun lastError(): String? = NativeBridge.nativeLastError()
}
