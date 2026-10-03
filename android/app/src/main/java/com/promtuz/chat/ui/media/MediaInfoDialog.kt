package com.promtuz.chat.ui.media

import com.promtuz.chat.ui.components.AppAlertDialog
import android.content.ClipData
import android.widget.Toast
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch

@Composable
fun MediaInfoDialog(item: MediaItem, onDismiss: () -> Unit) {
    val context = LocalContext.current
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    val summary = remember(context, item) { listOf(MediaInfoSection(null, mediaSummary(context, item))) }
    var loading by remember(item) { mutableStateOf(true) }
    var failed by remember(item) { mutableStateOf(false) }
    val details by produceState(summary, item) {
        loading = true
        failed = false
        value = summary
        try { value = readMediaDetails(context, item) }
        catch (cancel: CancellationException) { throw cancel }
        catch (_: Exception) { failed = true }
        finally { loading = false }
    }
    AppAlertDialog(onDismissRequest = onDismiss, title = { Text("Media info") }, text = {
        Column(Modifier.fillMaxWidth().padding(bottom = 16.dp), verticalArrangement = Arrangement.spacedBy(18.dp)) {
            SelectionContainer {
                Column(verticalArrangement = Arrangement.spacedBy(20.dp)) {
                    details.forEach { section ->
                        Column(verticalArrangement = Arrangement.spacedBy(14.dp)) {
                            section.title?.let { title ->
                                HorizontalDivider()
                                Text(title, style = MaterialTheme.typography.titleSmall, color = MaterialTheme.colorScheme.primary)
                            }
                            section.rows.forEach { (label, value) -> Column {
                                Text(label, style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
                                Text(value, style = MaterialTheme.typography.bodyLarge)
                            } }
                        }
                    }
                }
            }
            if (loading) CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
            if (failed) Text("Couldn’t read more details", style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }, confirmButton = { TextButton(onClick = onDismiss) { Text("Close") } }, dismissButton = {
        TextButton(onClick = {
            val text = details.joinToString("\n\n") { section ->
                listOfNotNull(section.title, section.rows.joinToString("\n") { (label, value) -> "$label: $value" }).joinToString("\n")
            }
            scope.launch {
                clipboard.setClipEntry(ClipEntry(ClipData.newPlainText("Media info", text)))
                Toast.makeText(context, "Media info copied", Toast.LENGTH_SHORT).show()
            }
        }) { Text("Copy") }
    })
}
