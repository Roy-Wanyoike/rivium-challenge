package com.droidcon.digger

import android.Manifest
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.AssistChip
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Tab
import androidx.compose.material3.TabRow
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp

/** The droidcon Kenya speakers directory: digests here also yield the list. */
private const val SPEAKERS_URL = "https://droidcon.co.ke/speakers"

/**
 * Compose front-end for the digger: enqueue URLs, watch ledger transitions
 * streamed from Rust via JobEvents.onJobUpdate, and browse the archived
 * droidcon speaker list (JobEvents.onSpeakers).
 */
class MainActivity : ComponentActivity() {

    private val askNotifications =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        AppContextHolder.install(applicationContext)
        RustHost.start(applicationContext)
        DigestStore.hydrate(applicationContext)
        if (Build.VERSION.SDK_INT >= 33) {
            askNotifications.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
        setContent { DiggerApp() }
    }

    override fun onDestroy() {
        // Keep the host alive across configuration changes; only the process
        // teardown ends it. Stop is available from RustHost.stop().
        super.onDestroy()
    }
}

private enum class DiggerTab(val label: String) {
    Digests("Digests"),
    Speakers("Speakers"),
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun DiggerApp() {
    var tab by remember { mutableStateOf(DiggerTab.Digests) }
    MaterialTheme {
        Scaffold(
            topBar = { TopAppBar(title = { Text("session-digger · droidcon Kenya") }) },
        ) { padding ->
            Column(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(padding),
            ) {
                TabRow(selectedTabIndex = tab.ordinal) {
                    DiggerTab.entries.forEach { entry ->
                        Tab(
                            selected = tab == entry,
                            onClick = { tab = entry },
                            text = { Text(entry.label) },
                        )
                    }
                }
                when (tab) {
                    DiggerTab.Digests -> Column(
                        modifier = Modifier
                            .fillMaxSize()
                            .padding(16.dp),
                        verticalArrangement = Arrangement.spacedBy(12.dp),
                    ) {
                        EnqueueForm()
                        JobList()
                    }

                    DiggerTab.Speakers -> SpeakersScreen()
                }
            }
        }
    }
}

@Composable
private fun EnqueueForm() {
    var url by remember { mutableStateOf("https://droidcon.co.ke") }
    val hostStatus by RustHost.status.collectAsState()

    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        OutlinedTextField(
            value = url,
            onValueChange = { url = it },
            modifier = Modifier.weight(1f),
            singleLine = true,
            label = { Text("URL to digest") },
        )
        Button(
            onClick = { RustHost.enqueue(url.trim()) },
            enabled = hostStatus == RustHost.STATUS_RUNNING && url.startsWith("http"),
        ) {
            Text("Dig")
        }
    }
}

@Composable
private fun JobList() {
    val rows by JobEvents.jobs.collectAsState()

    LazyColumn(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        if (rows.isEmpty()) {
            item {
                Text(
                    "No jobs yet.\nPaste a droidcon.co.ke link above, or add the " +
                        "Digger tile in Quick Settings and tap it.",
                    style = MaterialTheme.typography.bodyMedium,
                )
            }
        }
        items(rows, key = { it.id }) { row ->
            JobCard(row)
        }
    }
}

@Composable
private fun JobCard(row: JobRow) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Row(horizontalArrangement = Arrangement.SpaceBetween) {
                AssistChip(
                    onClick = {},
                    label = { Text(row.state.uppercase()) },
                )
                Text(
                    "attempt ${row.attempts}",
                    style = MaterialTheme.typography.labelSmall,
                )
            }
            Text(
                row.url,
                style = MaterialTheme.typography.titleSmall,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            if (!row.title.isNullOrBlank()) {
                Text(row.title!!, style = MaterialTheme.typography.bodyMedium)
            }
            if (!row.sha256.isNullOrBlank()) {
                Text(
                    "sha256 ${row.sha256}",
                    style = MaterialTheme.typography.bodySmall,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            if (!row.error.isNullOrBlank()) {
                Text(
                    row.error!!,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.error,
                )
            }
        }
    }
}

@Composable
private fun SpeakersScreen() {
    val speakers by JobEvents.speakers.collectAsState()
    val hostStatus by RustHost.status.collectAsState()

    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Row(
            horizontalArrangement = Arrangement.spacedBy(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Button(
                onClick = { RustHost.enqueue(SPEAKERS_URL) },
                enabled = hostStatus == RustHost.STATUS_RUNNING,
            ) {
                Text("Load speaker list")
            }
            Text(
                if (speakers.isEmpty()) {
                    "the directory is one digest away"
                } else {
                    "${speakers.size} speakers archived"
                },
                style = MaterialTheme.typography.bodySmall,
            )
        }
        if (speakers.isEmpty()) {
            Text(
                "No speakers yet.\nTap “Load speaker list” and the Rust worker digests " +
                    "droidcon.co.ke/speakers: every speaker's name and role is harvested " +
                    "into the ledger and shown here (the page still gets its usual " +
                    "title + SHA-256 record).",
                style = MaterialTheme.typography.bodyMedium,
            )
        } else {
            LazyColumn(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                items(speakers, key = { it.name }) { speaker ->
                    SpeakerCard(speaker)
                }
            }
        }
    }
}

@Composable
private fun SpeakerCard(speaker: Speaker) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Row(
            modifier = Modifier.padding(12.dp),
            horizontalArrangement = Arrangement.spacedBy(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                modifier = Modifier
                    .size(40.dp)
                    .clip(CircleShape)
                    .background(avatarColor(speaker.name)),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    speaker.name.take(1).uppercase(),
                    style = MaterialTheme.typography.titleMedium,
                    color = Color.White,
                )
            }
            Column {
                Text(speaker.name, style = MaterialTheme.typography.titleSmall)
                if (!speaker.role.isNullOrBlank()) {
                    Text(
                        speaker.role!!,
                        style = MaterialTheme.typography.bodySmall,
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
        }
    }
}

private val avatarPalette: List<Long> = listOf(
    0xFF0B57D0,
    0xFF146C2E,
    0xFF8C4A03,
    0xFF6B1E9E,
    0xFFA30E22,
    0xFF00696F,
)

/** Deterministic per-speaker accent for the initial circle. */
private fun avatarColor(name: String): Color =
    Color(avatarPalette[(name.hashCode() and Int.MAX_VALUE) % avatarPalette.size])
