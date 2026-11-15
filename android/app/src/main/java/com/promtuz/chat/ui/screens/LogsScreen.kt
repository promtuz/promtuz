package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.components.TextButton
import com.promtuz.chat.ui.components.Button
import com.promtuz.chat.ui.components.OutlinedButton
import android.content.ClipData
import android.content.Context
import android.content.Intent
import android.os.Build
import android.widget.Toast
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
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
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.unit.sp
import com.promtuz.chat.ui.components.Radii
import com.promtuz.chat.ui.components.RowSubtitleStyle
import com.promtuz.chat.ui.components.SegmentedTabs
import com.promtuz.chat.ui.theme.Promtuz
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.FilterChip
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
import com.promtuz.chat.ui.components.SearchableHeader
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
    var tab by rememberSaveable { mutableStateOf(0) }
    val severity = Tabs[tab].second
    val source = paused ?: live
    val logs = remember(source, query, severity) {
        source.filter { log -> log.priority >= severity && (query.isBlank() ||
            "${log.tag.orEmpty()} ${log.message} ${log.t?.stackTraceToString().orEmpty()}".contains(query.trim(), true)) }
    }
    val listState = rememberLazyListState()
    val back = LocalOnBackPressedDispatcherOwner.current?.onBackPressedDispatcher
    ScreenScaffold(scrollableState = listState, containerColor = Promtuz.colors.groupedPage, topBar = { scroll ->
        SearchableHeader(
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
        Column(Modifier.fillMaxSize().padding(top = padding.calculateTopPadding() + 6.dp)) {
            SegmentedTabs(Tabs.map { it.first }, tab, Modifier.padding(horizontal = 12.dp)) { tab = it }
            if (paused != null) Text("Updates paused", Modifier.padding(start = 28.dp, end = 28.dp, top = 10.dp),
                style = RowSubtitleStyle, color = Promtuz.colors.muted)
            if (logs.isEmpty()) Text(if (query.isNotBlank() || severity > 0) "No matching logs" else "No logs yet",
                Modifier.padding(horizontal = 28.dp, vertical = 16.dp), style = RowSubtitleStyle, color = Promtuz.colors.muted)
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
    val c = Promtuz.colors
    // Newest first with a reversed layout: the list reads top to bottom in time and follows new lines.
    LazyColumn(
        modifier.fillMaxWidth().fillMaxHeight(),
        contentPadding = PaddingValues(start = 12.dp, end = 12.dp, top = 12.dp, bottom = padding.calculateBottomPadding() + 12.dp),
        verticalArrangement = Arrangement.Top,
        state = listState,
        reverseLayout = true,
    ) {
        itemsIndexed(logs, key = { _, it -> it.id }) { i, log ->
            val top = i == logs.lastIndex
            val bottom = i == 0
            val r = Radii.card
            SelectionContainer(
                Modifier.fillMaxWidth()
                    .clip(RoundedCornerShape(if (top) r else 0.dp, if (top) r else 0.dp, if (bottom) r else 0.dp, if (bottom) r else 0.dp))
                    .background(c.card)
                    .padding(start = 16.dp, end = 16.dp, top = if (top) 14.dp else 5.dp, bottom = if (bottom) 14.dp else 5.dp),
            ) { LogEntry(log) }
        }
    }
}

@Composable
fun LogEntry(log: AppLog) {
    val c = Promtuz.colors
    val tone = when {
        log.priority >= 6 -> c.danger
        log.priority == 5 -> c.warn
        else -> c.accent
    }
    Column(verticalArrangement = Arrangement.spacedBy(3.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Text(formatTime(log.time), style = LogMeta, color = c.muted)
            log.tag?.takeIf { it.isNotBlank() }?.let {
                Text(it, Modifier.clip(RoundedCornerShape(5.dp)).background(tone.copy(alpha = 0.16f)).padding(horizontal = 5.dp, vertical = 1.dp),
                    style = LogMeta, color = tone)
            }
        }
        Text(log.message, style = LogText, color = c.text)
        log.t?.let { t ->
            // Traces run to dozens of lines; a few show until the line is tapped.
            var open by rememberSaveable(log.id) { mutableStateOf(false) }
            val trace = remember(t) { t.stackTraceToString().trimEnd().lines().map(String::trim) }
            val shown = if (open || trace.size <= TraceLines + 1) trace else trace.take(TraceLines)
            Text(
                shown.joinToString("\n") + if (shown.size < trace.size) "\n… ${trace.size - shown.size} more lines" else "",
                Modifier.clickable(onClickLabel = if (open) "Collapse" else "Expand") { open = !open },
                style = LogMeta, color = c.danger.copy(alpha = 0.85f),
            )
        }
    }
}

private const val TraceLines = 4
private val Tabs = listOf("All" to 0, "Errors" to 6, "Info" to 4)
private val LogText = TextStyle(fontFamily = FontFamily.Monospace, fontSize = 13.sp, lineHeight = 19.sp)
private val LogMeta = TextStyle(fontFamily = FontFamily.Monospace, fontSize = 12.sp, lineHeight = 16.sp)

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
