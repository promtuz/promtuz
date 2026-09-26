package com.promtuz.chat.ui.screens

import android.content.Intent
import android.net.Uri
import android.text.format.Formatter
import androidx.compose.foundation.Image
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.Modifier
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.core.content.FileProvider
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.media.*
import com.promtuz.chat.utils.extensions.*
import com.promtuz.chat.utils.media.*
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.Dispatchers
import java.io.File
import java.text.DateFormat
import java.util.Date

@Composable
fun SharedMediaScreen(conversation: String, name: String) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var error by remember { mutableStateOf<String?>(null) }
    val rows by remember(conversation) {
        observeQuery(setOf("messages", "message_media", "partials")) { CoreBridge.sharedMedia(conversation.fromHex()) }
    }.collectAsState(null)
    fun category(r: uniffi.core.SharedMediaItem) = when {
        r.kind.toInt() == 3 -> "Voice"
        r.kind.toInt() == 4 -> "Stickers"
        r.mime.startsWith("image/") || r.mime.startsWith("video/") -> "Media"
        else -> "Files"
    }
    val tabs = listOf("Media", "Files", "Voice", "Stickers").filter { t -> rows.orEmpty().any { category(it) == t } }
    var chosen by rememberSaveable { mutableStateOf("Media") }
    val tab = chosen.takeIf { it in tabs } ?: tabs.firstOrNull()
    SimpleScreen({ Text("Shared media") }) { padding ->
        Column(Modifier.fillMaxSize().padding(top = padding.calculateTopPadding())) {
            if (tabs.size > 1) PrimaryScrollableTabRow(selectedTabIndex = tabs.indexOf(tab).coerceAtLeast(0)) {
                tabs.forEach { t -> Tab(selected = tab == t, onClick = { chosen = t }, text = { Text(t) }) }
            }
            if (rows == null) CircularProgressIndicator(Modifier.padding(24.dp))
            else if (tabs.isEmpty()) Text("No shared media", Modifier.padding(24.dp), color = MaterialTheme.colorScheme.onSurfaceVariant)
            error?.let { Text(it, Modifier.padding(16.dp), color = MaterialTheme.colorScheme.error) }
            LazyColumn(contentPadding = PaddingValues(bottom = padding.calculateBottomPadding() + 24.dp)) {
                items(rows.orEmpty().filter { category(it) == tab }, key = { it.dispatchId.toHex() }) { row ->
                    val did = row.dispatchId.toHex()
                    val record by produceState<uniffi.core.MediaRecord?>(null, did, rows) {
                        value = runCatching { CoreBridge.getMessageMedia(conversation.fromHex(), row.dispatchId) }.getOrNull()
                    }
                    val thumb by produceState<androidx.compose.ui.graphics.ImageBitmap?>(null, did, record) {
                        val bytes = record?.let { if (it.kind.toInt() == 1) it.blob else it.thumb }
                        value = bytes?.let { withContext(Dispatchers.Default) { decodeAvif(it, 160) } }
                    }
                    if (row.kind.toInt() == 3 && record?.blob != null) {
                        Column(Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 12.dp)) {
                            Text(DateFormat.getDateInstance().format(Date(row.timestamp.toLong() * 1000)), style = MaterialTheme.typography.labelMedium)
                            VoiceBlock(MessageContent.Voice(did, row.mime, record!!.durationMs.toInt(), record!!.thumb ?: byteArrayOf(), record!!.blob!!),
                                MaterialTheme.colorScheme.onSurface)
                        }
                    } else ListItem(
                        modifier = Modifier.clickable {
                            scope.launch {
                                error = null
                                try {
                                    val media = CoreBridge.getMessageMedia(conversation.fromHex(), row.dispatchId) ?: error("Attachment unavailable")
                                    if (media.kind.toInt() == 1) {
                                        val image = media.blob?.let { withContext(Dispatchers.Default) { decodeAvif(it) } } ?: error("Image unavailable")
                                        MediaViewer.open(listOf(pictureItem(did, image, row.senderName.ifBlank { name }).copy(mime = media.mime,
                                            subtitle = DateFormat.getDateTimeInstance().format(Date(row.timestamp.toLong() * 1000)), byteSize = media.blob?.size?.toLong())))
                                    } else if (media.kind.toInt() == 4 && media.sticker != null) {
                                        val image = withContext(Dispatchers.Default) { decodeAvif(CoreBridge.stickerImage(media.sticker!!)) } ?: error("Sticker unavailable")
                                        MediaViewer.open(listOf(pictureItem(did, image, row.senderName.ifBlank { name })))
                                    } else if (media.localPath != null) {
                                        val file = File(media.localPath!!)
                                        if (media.mime.startsWith("video/") || media.mime.startsWith("image/")) {
                                            MediaViewer.open(listOf(MediaItem(did, thumb, media.width.toInt().coerceAtLeast(1), media.height.toInt().coerceAtLeast(1),
                                                title = row.senderName.ifBlank { name }, subtitle = DateFormat.getDateTimeInstance().format(Date(row.timestamp.toLong() * 1000)), mime = media.mime, filePath = file.absolutePath,
                                                shareName = media.name, byteSize = media.size.toLong(),
                                                videoPath = file.absolutePath.takeIf { media.mime.startsWith("video/") },
                                                load = { decodeDownscaled(context, Uri.fromFile(file), 4096)?.let { it.asImageBitmap() } })))
                                        } else {
                                            val uri = FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", file)
                                            context.startActivity(Intent(Intent.ACTION_VIEW).setDataAndType(uri, media.mime).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION))
                                        }
                                    } else if (media.fileId != null) {
                                        CoreBridge.downloadAttachment(media.fileId!!)
                                        error = "Download requested"
                                    } else error("Attachment unavailable")
                                } catch (e: kotlinx.coroutines.CancellationException) { throw e }
                                catch (_: Exception) { error = "Couldn't open this attachment" }
                            }
                        },
                        leadingContent = {
                            if (thumb != null) Image(thumb!!, null, Modifier.size(64.dp), contentScale = ContentScale.Crop)
                            else DrawableIcon(when (tab) { "Media" -> R.drawable.oi_image; "Stickers" -> R.drawable.oi_sticker; else -> R.drawable.oi_file_attachment }, size = 32.dp)
                        },
                        headlineContent = { Text(row.name.ifBlank { if (row.kind.toInt() == 4) "Sticker" else if (row.mime.startsWith("video/")) "Video" else "Photo" }, maxLines = 2) },
                        supportingContent = { Text(listOfNotNull(
                            row.size.toLong().takeIf { it > 0 }?.let { Formatter.formatShortFileSize(context, it) },
                            DateFormat.getDateInstance().format(Date(row.timestamp.toLong() * 1000))
                        ).joinToString(" · ")) },
                    )
                }
            }
        }
    }
}
