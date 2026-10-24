package com.droidcon.digger

import android.Manifest
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.AssistChip
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp

/**
 * Compose front-end for the digger: enqueue URLs, watch ledger transitions
 * streamed from Rust via JobEvents.onJobUpdate.
 */
class MainActivity : ComponentActivity() {

    private val askNotifications =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        AppContextHolder.install(applicationContext)
        RustHost.start(applicationContext)
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

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun DiggerApp() {
    MaterialTheme {
        Scaffold(
            topBar = { TopAppBar(title = { Text("session-digger · droidcon Kenya") }) },
        ) { padding ->
            Column(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(padding)
                    .padding(16.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                EnqueueForm()
                JobList()
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
                    overflow = TextOverflow.MiddleEllipsis,
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
