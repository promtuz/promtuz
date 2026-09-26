package com.promtuz.chat.ui.media

import com.promtuz.chat.ui.components.AppAlertDialog
import android.graphics.BitmapFactory
import android.media.MediaMetadataRetriever
import android.text.format.Formatter
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File

@Composable
fun MediaInfoDialog(item: MediaItem, onDismiss: () -> Unit) {
    val context = LocalContext.current
    val details by produceState<List<Pair<String, String>>>(emptyList(), item.key) {
        value = withContext(Dispatchers.IO) {
            buildList {
                if (item.title.isNotBlank()) add("From" to item.title)
                if (item.subtitle.isNotBlank()) add("Date" to item.subtitle)
                if (item.filePath != null) add("Name" to item.shareName)
                add("Type" to item.mime)
                (item.byteSize ?: item.filePath?.let { File(it).takeIf(File::isFile)?.length() })
                    ?.let { add("Size" to Formatter.formatShortFileSize(context, it)) }
                var width = if (item.filePath == null) item.width else 0
                var height = if (item.filePath == null) item.height else 0
                if (item.videoPath != null) runCatching {
                    val r = MediaMetadataRetriever()
                    try {
                        r.setDataSource(item.videoPath)
                        width = r.extractMetadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_WIDTH)?.toIntOrNull() ?: 0
                        height = r.extractMetadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_HEIGHT)?.toIntOrNull() ?: 0
                        r.extractMetadata(MediaMetadataRetriever.METADATA_KEY_DURATION)?.toLongOrNull()?.let {
                            val seconds = it / 1000
                            add("Duration" to "%d:%02d".format(seconds / 60, seconds % 60))
                        }
                    } finally { r.release() }
                } else if (item.filePath != null) runCatching {
                    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
                    BitmapFactory.decodeFile(item.filePath, bounds)
                    width = bounds.outWidth; height = bounds.outHeight
                }
                if (width > 0 && height > 0) add("Dimensions" to "$width × $height")
            }
        }
    }
    AppAlertDialog(onDismissRequest = onDismiss, title = { Text("Media info") }, text = {
        Column(verticalArrangement = Arrangement.spacedBy(14.dp)) {
            details.forEach { (label, value) -> Column {
                Text(label, style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
                Text(value, style = MaterialTheme.typography.bodyLarge)
            } }
        }
    }, confirmButton = { TextButton(onClick = onDismiss) { Text("Close") } })
}
