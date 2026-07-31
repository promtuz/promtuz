package com.promtuz.chat.ui.screens

import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.BackupLogLevel
import com.promtuz.chat.presentation.viewmodel.BackupLogLine
import com.promtuz.chat.presentation.viewmodel.BackupRestoreVM
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.SimpleScreen
import org.koin.androidx.compose.koinViewModel

/**
 * Developer utility: take a backup snapshot, export it off-device, and merge a
 * `.pzbk` back in — every step narrated in the console below, because the
 * production recovery path is silent by design and a silent failure there is
 * indistinguishable from success.
 *
 * Restore is additive: it can only add rows it doesn't already have. See
 * [BackupRestoreVM].
 */
@Composable
fun BackupRestoreScreen(viewModel: BackupRestoreVM = koinViewModel()) {
    val console by viewModel.console.collectAsState()
    val busy by viewModel.busy.collectAsState()

    val saveCopy = rememberLauncherForActivityResult(
        ActivityResultContracts.CreateDocument("application/octet-stream")
    ) { uri -> uri?.let(viewModel::saveCopyTo) }

    val pickBackup = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenDocument()
    ) { uri -> uri?.let(viewModel::restoreFrom) }

    SimpleScreen({ Text("Backup & Restore") }, actions = {
        IconButton(viewModel::clearConsole) {
            DrawableIcon(R.drawable.oi_clear_list, desc = "Clear console", size = 20.dp)
        }
    }) { padding ->
        Column(
            Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(horizontal = 16.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Notice()

            Row(
                Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                Button(
                    viewModel::snapshot,
                    Modifier.weight(1f),
                    enabled = !busy,
                ) { Text("Take snapshot") }
                OutlinedButton(
                    { saveCopy.launch(viewModel.suggestedFileName()) },
                    Modifier.weight(1f),
                    enabled = !busy,
                ) { Text("Save a copy") }
            }

            OutlinedButton(
                { pickBackup.launch(arrayOf("*/*")) },
                Modifier.fillMaxWidth(),
                enabled = !busy,
            ) {
                if (busy) {
                    CircularProgressIndicator(Modifier.size(18.dp))
                } else {
                    Text("Restore from a .pzbk file")
                }
            }

            Console(console, Modifier.weight(1f))
        }
    }
}

/** States the two rules that make this screen safe to hand to a tester. */
@Composable
private fun Notice() {
    val colors = MaterialTheme.colorScheme
    Column(
        Modifier
            .fillMaxWidth()
            .clip(MaterialTheme.shapes.large)
            .background(colors.surfaceContainerLow)
            .padding(14.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            Icon(
                painterResource(R.drawable.i_encrypted),
                null,
                Modifier.size(18.dp),
                tint = colors.onSurfaceVariant,
            )
            Text(
                "Restore only adds what's missing",
                style = MaterialTheme.typography.labelLarge,
                color = colors.onSurface,
            )
        }
        Text(
            "Nothing already on this device is replaced, renamed or deleted. A backup " +
                "decrypts under your own key, so its contents can be edited by whoever " +
                "holds it — a live message always wins over the file.",
            style = MaterialTheme.typography.bodySmall,
            color = colors.onSurfaceVariant,
        )
    }
}

@Composable
private fun Console(lines: List<BackupLogLine>, modifier: Modifier = Modifier) {
    val state = rememberLazyListState()

    // Follow the tail: every appended line scrolls into view.
    LaunchedEffect(lines.size) {
        if (lines.isNotEmpty()) state.animateScrollToItem(lines.lastIndex)
    }

    LazyColumn(
        modifier
            .fillMaxWidth()
            .clip(MaterialTheme.shapes.largeIncreased)
            .background(MaterialTheme.colorScheme.surfaceContainerLow)
            .padding(10.dp),
        state = state,
        verticalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        items(lines, key = { it.id }) { l ->
            SelectionContainer {
                Text(
                    l.text,
                    style = MaterialTheme.typography.bodySmall,
                    fontFamily = FontFamily.Monospace,
                    fontWeight = if (l.level == BackupLogLevel.STEP) FontWeight.Bold else null,
                    color = levelColor(l.level),
                )
            }
        }
    }
}

/** Same palette the App Logs console uses, so severity reads identically. */
@Composable
private fun levelColor(level: BackupLogLevel): Color = when (level) {
    BackupLogLevel.STEP -> MaterialTheme.colorScheme.onSurface
    BackupLogLevel.INFO -> MaterialTheme.colorScheme.onSurfaceVariant
    BackupLogLevel.OK -> Color(0xFF66BB6A)
    BackupLogLevel.WARN -> Color(0xFFFFA726)
    BackupLogLevel.ERR -> Color(0xFFEF5350)
}
