package com.promtuz.chat.ui.screens

import android.content.ClipData
import android.content.Context
import android.content.Intent
import android.os.Build
import android.widget.Toast
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.shape.CornerSize
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.Button
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
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
import com.promtuz.chat.BuildConfig
import com.promtuz.chat.R
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.utils.logs.AppLog
import com.promtuz.chat.utils.logs.AppLogger
import kotlinx.coroutines.launch
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

@Composable
fun LogsScreen() {
    var showExport by remember { mutableStateOf(false) }

    SimpleScreen({ Text("App Logs") }, actions = {
        IconButton({ showExport = true }) {
            DrawableIcon(R.drawable.i_send, desc = "Export logs")
        }
    }) { padding ->
        LogsContainer(Modifier, padding)
    }

    if (showExport) LogExportSheet { showExport = false }
}

@Composable
private fun LogExportSheet(onDismiss: () -> Unit) {
    val context = LocalContext.current
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    val logs by AppLogger.logs.collectAsState()

    ModalBottomSheet(onDismissRequest = onDismiss) {
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
    val file = File(context.cacheDir, "logs").apply { mkdirs() }.resolve("promtuz-logs.txt")
    file.writeText(text)
    val uri = FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", file)
    val send = Intent(Intent.ACTION_SEND).apply {
        type = "text/plain"
        putExtra(Intent.EXTRA_STREAM, uri)
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    }
    context.startActivity(Intent.createChooser(send, "Export logs"))
}

/** Header (build/device) + chronological body, one tight line per entry, stacktraces indented. */
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
    modifier: Modifier = Modifier,
    padding: PaddingValues = PaddingValues(0.dp),
) {
    val logs by AppLogger.logs.collectAsState()

    LazyColumn(
        modifier
            .padding(padding)
            .padding(horizontal = 16.dp)
            .clip(MaterialTheme.shapes.largeIncreased)
            .background(MaterialTheme.colorScheme.surfaceContainerLow)
            .fillMaxWidth()
            .fillMaxHeight()
            .padding(3.dp),
        verticalArrangement = Arrangement.spacedBy(1.dp, Alignment.Bottom),
        reverseLayout = true
    ) {
        itemsIndexed(logs, key = { _, it -> it.id }) { i, log ->
            SelectionContainer(Modifier.fillMaxWidth()) {
                LogEntry(log, i == 0)
            }
        }
    }
}

@Composable
fun LogEntry(log: AppLog, isLast: Boolean = false) {
    val color = when (prioLabel(log.priority)) {
        "V" -> Color(0xFF9E9E9E)
        "D" -> Color(0xFF42A5F5)
        "I" -> Color(0xFF66BB6A)
        "W" -> Color(0xFFFFA726)
        "E" -> Color(0xFFEF5350)
        "F" -> Color(0xFFAB47BC)
        else -> Color.Unspecified
    }

    val cz0 = CornerSize(0)

    Column(
        Modifier
            .let { mfr ->
                if (isLast) mfr.clip(
                    // FIXME: innerRounding = outerRounding - padding
                    MaterialTheme.shapes.largeIncreased.copy(
                        topEnd = cz0,
                        topStart = cz0
                    )
                ) else mfr
            }
            .background(color.copy(0.15f))
            .fillMaxWidth()
            .padding(5.dp, 2.dp)
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
