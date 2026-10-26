package com.droidcon.digger

import android.service.quicksettings.Tile
import android.service.quicksettings.TileService

/**
 * droidcon digest QS tile.
 *
 * State:
 * - active while the Rust host is running;
 * - subtitle shows the pending queue size (queued + running);
 * - a tap enqueues the droidcon Kenya homepage for digestion.
 */
class DigTileService : TileService() {

    override fun onStartListening() {
        super.onStartListening()
        render()
    }

    override fun onClick() {
        super.onClick()
        val hostRunning = RustHost.statusNow() == RustHost.STATUS_RUNNING
        if (hostRunning) {
            RustHost.enqueue(DEFAULT_URL)
        }
        render()
    }

    private fun render() {
        val tile = qsTile ?: return
        val hostRunning = RustHost.statusNow() == RustHost.STATUS_RUNNING
        tile.state = if (hostRunning) Tile.STATE_ACTIVE else Tile.STATE_INACTIVE
        val pending = JobEvents.pendingCount()
        tile.subtitle = if (pending > 0) "$pending in queue" else "tap to dig droidcon.co.ke"
        tile.updateTile()
    }

    private companion object {
        const val DEFAULT_URL = "https://droidcon.co.ke"
    }
}
