package com.promtuz.chat.ui.screens

import android.content.ClipData
import android.content.Context
import android.content.Intent
import android.os.Build
import android.widget.Toast
import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.FilterChip
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.core.content.FileProvider
import com.promtuz.chat.ui.components.AppBottomSheet
import com.promtuz.chat.BuildConfig
import com.promtuz.chat.R
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.ScreenScaffold
import com.promtuz.chat.ui.components.ContactPickerHeader
import com.promtuz.chat.ui.components.AppDropMenu
import com.promtuz.chat.ui.components.MenuAction
import com.promtuz.chat.ui.components.MorphGlyph
import com.promtuz.chat.ui.components.AppAlertDialog
import androidx.activity.compose.LocalOnBackPressedDispatcherOwner
import com.promtuz.chat.utils.logs.AppLog
import com.promtuz.chat.utils.logs.AppLogger
import kotlinx.coroutines.launch
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

@Composable
fun LogsScreen() {
    val live by AppLogger.logs.collectAsState()
    var paused by remember { mutableStateOf<List<AppLog>?>(null) }
    var export by remember { mutableStateOf<List<AppLog>?>(null) }
    var confirmClear by remember { mutableStateOf(false) }
    var searching by rememberSaveable { mutableStateOf(false) }
    var query by rememberSaveable { mutableStateOf("") }
    var severity by rememberSaveable { mutableStateOf(0) }
    val source = paused ?: live
    val logs = remember(source, query, severity) {
        source.filter { log -> log.priority >= severity && (query.isBlank() ||
            "${log.tag.orEmpty()} ${log.message} ${log.t?.stackTraceToString().orEmpty()}".contains(query.trim(), true)) }
    }
    val listState = rememberLazyListState()
    val back = LocalOnBackPressedDispatcherOwner.current?.onBackPressedDispatcher
    ScreenScaffold(scrollableState = listState, topBar = { scroll ->
        ContactPickerHeader(
            title = "App logs", close = false, searching = searching, query = query,
            onQuery = { query = it }, onSearch = { searching = true }, searchLabel = "Search logs",
            onBack = { if (searching) { searching = false; query = "" } else back?.onBackPressed() },
            scrollBehavior = scroll, windowInsets = TopAppBarDefaults.windowInsets,
            showActionsWhileSearching = true,
            actions = {
                AppDropMenu(anchor = { DrawableIcon(R.drawable.i_more_vert, Modifier.padding(12.dp), desc = "Log options") },
                    groups = buildList {
                        add(listOf(MenuAction(if (paused == null) "Pause updates" else "Resume updates",
                            glyph = if (paused == null) MorphGlyph.Pause else MorphGlyph.Play) {
                            paused = if (paused == null) live.toList() else null
                        }))
                        if (logs.isNotEmpty()) add(listOf(MenuAction("Export logs", R.drawable.oi_export) { export = logs.toList() }))
                        if (live.isNotEmpty() || paused?.isNotEmpty() == true)
                            add(listOf(MenuAction("Clear logs", R.drawable.oi_broom, destructive = true) { confirmClear = true }))
                    })
            },
        )
    }) { padding ->
        Column(Modifier.fillMaxSize().padding(top = padding.calculateTopPadding())) {
            Row(Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()).padding(horizontal = 16.dp),
                horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                listOf("All" to 0, "Info" to 4, "Warnings" to 5, "Errors" to 6).forEach { (label, value) ->
                    FilterChip(selected = severity == value, onClick = { severity = value }, label = { Text(label) })
                }
            }
            if (paused != null) Text("Updates paused", Modifier.padding(horizontal = 20.dp, vertical = 8.dp),
                color = MaterialTheme.colorScheme.onSurfaceVariant, style = MaterialTheme.typography.labelMedium)
            if (logs.isEmpty()) Text(if (query.isNotBlank() || severity > 0) "No matching logs" else "No logs yet",
                Modifier.padding(20.dp), color = MaterialTheme.colorScheme.onSurfaceVariant)
            else LogsContainer(logs, Modifier.weight(1f), PaddingValues(bottom = padding.calculateBottomPadding()), listState)
        }
    }
    export?.let { LogExportSheet(it) { export = null } }
    if (confirmClear) AppAlertDialog(onDismissRequest = { confirmClear = false },
        title = { Text("Clear logs?") }, text = { Text("This removes the current diagnostic logs. Your chats and settings stay on this device.") },
        confirmButton = { TextButton(onClick = { AppLogger.clear(); paused = paused?.let { emptyList() }; confirmClear = false }) { Text("Clear") } },
        dismissButton = { TextButton(onClick = { confirmClear = false }) { Text("Cancel") } })
}

@Composable
private fun LogExportSheet(logs: List<AppLog>, onDismiss: () -> Unit) {
    val context = LocalContext.current
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()

    AppBottomSheet(onDismissRequest = onDismiss) {
        Column(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 24.dp)
                .padding(bottom = 32.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text("Export logs", style = MaterialTheme.typography.titleMedium)
            Button(
                onClick = {
                    val text = formatLogs(logs)
                    scope.launch {
                        clipboard.setClipEntry(ClipEntry(ClipData.newPlainText("Promtuz logs", text)))
                        Toast.makeText(context, "Logs copied", Toast.LENGTH_SHORT).show()
                        onDismiss()
                    }
                },
                Modifier.fillMaxWidth(),
            ) { Text("Copy to clipboard") }
            OutlinedButton(
                onClick = {
                    shareLogFile(context, formatLogs(logs))
                    onDismiss()
                },
                Modifier.fillMaxWidth(),
            ) { Text("Export as file") }
        }
    }
}

private fun shareLogFile(context: Context, text: String) {
    val file = File.createTempFile("promtuz-logs-", ".txt", File(context.cacheDir, "logs").apply { mkdirs() })
    file.writeText(text)
    val uri = FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", file)
    val send = Intent(Intent.ACTION_SEND).apply {
        type = "text/plain"
        putExtra(Intent.EXTRA_STREAM, uri)
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    }
    context.startActivity(Intent.createChooser(send, "Export logs"))
}

private fun formatLogs(logs: List<AppLog>): String {
    val time = SimpleDateFormat("HH:mm:ss.SSS", Locale.ENGLISH)
    val stamp = SimpleDateFormat("yyyy-MM-dd HH:mm:ss", Locale.ENGLISH)
    return buildString {
        appendLine("app: ${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE})")
        appendLine("android: ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})")
        appendLine("device: ${Build.MANUFACTURER} ${Build.MODEL}")
        appendLine("abi: ${Build.SUPPORTED_ABIS.firstOrNull() ?: "?"}")
        appendLine("exported: ${stamp.format(Date())}")
        appendLine("=== PROMTUZ LOGS ===")
        // Stored newest-first; reverse so the file reads top-to-bottom chronologically.
        logs.asReversed().forEach { log ->
            val tag = log.tag?.takeIf { it.isNotBlank() }?.let { "[$it] " }.orEmpty()
            append(time.format(Date(log.time)))
            append("  ${prioLabel(log.priority)}  ")
            appendLine("$tag${log.message}")
            log.t?.let { t ->
                t.stackTraceToString().trimEnd().lineSequence().forEach { appendLine("    $it") }
            }
        }
    }
}

@Composable
fun LogsContainer(
    logs: List<AppLog>,
    modifier: Modifier = Modifier,
    padding: PaddingValues = PaddingValues(0.dp),
    listState: LazyListState = rememberLazyListState(),
) {
    LazyColumn(
        modifier
            .fillMaxWidth()
            .fillMaxHeight(),
        contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 8.dp, bottom = padding.calculateBottomPadding() + 12.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp, Alignment.Bottom),
        state = listState,
        reverseLayout = true
    ) {
        items(logs, key = { it.id }) { log ->
            SelectionContainer(Modifier.fillMaxWidth()) {
                LogEntry(log)
            }
        }
    }
}

@Composable
fun LogEntry(log: AppLog) {
    val color = when (prioLabel(log.priority)) {
        "V" -> Color(0xFF9E9E9E)
        "D" -> Color(0xFF42A5F5)
        "I" -> Color(0xFF66BB6A)
        "W" -> Color(0xFFFFA726)
        "E" -> Color(0xFFEF5350)
        "F" -> Color(0xFFAB47BC)
        else -> Color.Unspecified
    }

    Column(
        Modifier
            .fillMaxWidth()
            .clip(MaterialTheme.shapes.medium)
            .background(MaterialTheme.colorScheme.surfaceContainerLow)
            .padding(12.dp, 10.dp)
    ) {
        Row(verticalAlignment = Alignment.Top) {
            Text(
                text = prioLabel(log.priority),
                style = MaterialTheme.typography.labelSmall,
                color = color,
            )
            Spacer(Modifier.width(8.dp))
            Text(
                text = formatTime(log.time),
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.5f)
            )
            log.tag?.let {
                Spacer(Modifier.width(8.dp))
                Text(
                    text = "[$it]",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant
                )
            }
        }
        Text(
            text = log.message,
            style = MaterialTheme.typography.bodyMediumEmphasized,
            fontFamily = FontFamily.Monospace,
            color = MaterialTheme.colorScheme.onSurface
        )

        log.t?.let {
            Text(
                text = it.stackTraceToString(),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.error.copy(alpha = 0.8f)
            )
        }
    }
}

fun prioLabel(p: Int) = when (p) {
    2 -> "V"
    3 -> "D"
    4 -> "I"
    5 -> "W"
    6 -> "E"
    7 -> "F"
    8 -> "S"
    else -> p.toString()
}

fun formatTime(ts: Long): String =
    SimpleDateFormat("HH:mm:ss.SSS", Locale.ENGLISH).format(Date(ts))
