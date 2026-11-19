package com.promtuz.chat.ui.screens

import android.content.Context
import android.text.format.DateUtils
import android.text.format.Formatter
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.MessageLocation
import com.promtuz.chat.domain.model.mediaLabel
import com.promtuz.chat.domain.model.toRef
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.media.*
import com.promtuz.chat.ui.text.dateAndTime
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.utils.extensions.*
import com.promtuz.chat.utils.media.*
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.core.MediaRecord
import uniffi.core.SharedMediaItem
import java.util.Calendar

private enum class SharedTab(val label: String) { Media("Media"), Files("Files"), Voice("Voice"), Stickers("Stickers") }

private fun SharedMediaItem.tab() = when {
    kind.toInt() == 3 -> SharedTab.Voice
    kind.toInt() == 4 -> SharedTab.Stickers
    mime.startsWith("image/") || mime.startsWith("video/") -> SharedTab.Media
    else -> SharedTab.Files
}

private val SharedMediaItem.ms get() = timestamp.toLong() * 1000

private fun monthLabel(ms: Long): String {
    val then = Calendar.getInstance().apply { timeInMillis = ms }
    val sameYear = then.get(Calendar.YEAR) == Calendar.getInstance().get(Calendar.YEAR)
    return java.text.SimpleDateFormat(if (sameYear) "LLLL" else "LLLL yyyy", java.util.Locale.getDefault()).format(then.time)
}

@Composable
fun SharedMediaScreen(conversation: String, name: String) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val c = Promtuz.colors
    var error by remember { mutableStateOf<String?>(null) }
    val rows by remember(conversation) {
        observeQuery(setOf("messages", "message_media", "partials")) {
            try { CoreBridge.sharedMedia(conversation.fromHex()) }
            catch (error: Exception) {
                val id = conversation.fromHex()
                if (CoreBridge.privateConversationIds().any { it.contentEquals(id) } &&
                    !CoreBridge.isPrivateConversationUnlocked(id)) emptyList() else throw error
            }
        }
    }.collectAsState(null)
    val tabs = SharedTab.entries.filter { t -> rows.orEmpty().any { it.tab() == t } }
    var chosen by rememberSaveable { mutableStateOf(SharedTab.Media) }
    val tab = chosen.takeIf { it in tabs } ?: tabs.firstOrNull()
    val months = remember(rows, tab) {
        rows.orEmpty().filter { it.tab() == tab }.sortedByDescending { it.timestamp }.groupBy { monthLabel(it.ms) }.toList()
    }
    val open: (SharedMediaItem, ImageBitmap?) -> Unit = { row, thumb ->
        scope.launch {
            error = null
            try { openShared(context, conversation, name, row, thumb) }
            catch (e: kotlinx.coroutines.CancellationException) { throw e }
            catch (e: DownloadRequested) { error = Downloading }
            catch (_: Exception) { error = "Couldn't open this attachment" }
        }
    }
    val tabRow: @Composable () -> Unit = {
        if (tabs.size > 1) SegmentedTabs(tabs.map { it.label }, tabs.indexOf(tab).coerceAtLeast(0), Modifier.padding(bottom = 4.dp)) {
            chosen = tabs[it]
        }
    }
    val errorLine: @Composable () -> Unit = {
        error?.let { Text(it, Modifier.padding(horizontal = 16.dp, vertical = 4.dp), style = FootnoteStyle, color = if (it == Downloading) c.muted else c.danger) }
    }

    GroupedScreen({ Text("Shared with $name", maxLines = 1, overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis) },
        connectionStatus = false) { padding ->
        when {
            rows != null && tabs.isEmpty() -> Box(Modifier.fillMaxSize().padding(padding), Alignment.Center) {
                Text("Nothing shared yet", style = RowTitleStyle, color = c.muted)
            }
            tab == SharedTab.Media || tab == SharedTab.Stickers -> LazyVerticalGrid(
                GridCells.Fixed(if (tab == SharedTab.Stickers) 4 else 3),
                Modifier.fillMaxSize(),
                contentPadding = padding.listPadding(top = 8.dp),
                horizontalArrangement = Arrangement.spacedBy(3.dp),
                verticalArrangement = Arrangement.spacedBy(3.dp),
            ) {
                item("tabs", span = { GridItemSpan(maxLineSpan) }) { tabRow() }
                item("error", span = { GridItemSpan(maxLineSpan) }) { errorLine() }
                months.forEach { (month, items) ->
                    item("m:$month", span = { GridItemSpan(maxLineSpan) }) {
                        Text(month, Modifier.padding(start = 4.dp, top = 10.dp, bottom = 4.dp), style = MonthStyle, color = c.muted)
                    }
                    items(items, key = { it.dispatchId.toHex() }) { row -> GridCell(conversation, row, open) }
                }
            }
            else -> LazyColumn(Modifier.fillMaxSize(), contentPadding = padding.listPadding(top = 8.dp),
                verticalArrangement = Arrangement.spacedBy(0.dp)) {
                item("tabs") { tabRow() }
                item("error") { errorLine() }
                months.forEach { (month, items) -> listMonth(month, items, conversation, open) }
            }
        }
    }
}

private val MonthStyle = TextStyle(fontSize = 13.sp, lineHeight = 18.sp, fontWeight = FontWeight.Medium)

private fun LazyListScope.listMonth(month: String, items: List<SharedMediaItem>, conversation: String,
    open: (SharedMediaItem, ImageBitmap?) -> Unit) {
    val count = items.size + 1
    item("m:$month") {
        LazyCardHeader(month, count, Modifier.padding(top = 12.dp))
    }
    itemsIndexed(items, key = { _, it -> it.dispatchId.toHex() }) { i, row ->
        Box(Modifier.lazyCardRow(i + 1, count, dividerStart = 70.dp)) { ListCell(conversation, row, open) }
    }
}

@Composable
private fun rememberRecord(conversation: String, row: SharedMediaItem): State<MediaRecord?> {
    val did = row.dispatchId.toHex()
    return produceState(null, did) {
        value = runCatching { CoreBridge.getMessageMedia(conversation.fromHex(), row.dispatchId) }.getOrNull()
    }
}

@Composable
private fun rememberThumb(record: MediaRecord?): State<ImageBitmap?> = produceState(null, record) {
    val bytes = record?.let { if (it.kind.toInt() == 1) it.blob else it.thumb }
    value = bytes?.let { withContext(Dispatchers.Default) { decodeAvifThumb(it, 256) } }
}

@Composable
private fun GridCell(conversation: String, row: SharedMediaItem, open: (SharedMediaItem, ImageBitmap?) -> Unit) {
    val record by rememberRecord(conversation, row)
    val thumb by rememberThumb(record)
    val c = Promtuz.colors
    val animate = MediaViewer.session == null
    Box(Modifier.aspectRatio(1f).clip(RoundedCornerShape(4.dp))
        .background(if (row.tab() == SharedTab.Stickers) Color.Transparent else c.card)
        .clickable { open(row, thumb) }) {
        val media = record
        val sticker = media?.sticker
        val path = remember(conversation, media) { media?.let { PrivateMediaAccess.path(conversation, it) } }
        when {
            sticker != null -> StickerImage(sticker.toRef(), null, Modifier.fillMaxSize().padding(4.dp), animate = animate)
            media?.kind?.toInt() == 1 -> EncodedImage(bytes = media.blob, poster = thumb, contentDescription = null,
                modifier = Modifier.fillMaxSize(), contentScale = ContentScale.Crop, maxEdge = 384, animate = animate)
            media?.mime?.startsWith("image/") == true && path != null -> EncodedImage(filePath = path, poster = thumb,
                contentDescription = null, modifier = Modifier.fillMaxSize(), contentScale = ContentScale.Crop, maxEdge = 384, animate = animate)
            thumb != null -> Image(thumb!!, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop)
        }
        if (row.mime.startsWith("video/") && media != null && media.durationMs > 0u) Text(
            DateUtils.formatElapsedTime(media.durationMs.toLong() / 1000),
            Modifier.align(Alignment.BottomStart).padding(6.dp).clip(RoundedCornerShape(6.dp))
                .background(Color.Black.copy(alpha = 0.5f)).padding(horizontal = 5.dp, vertical = 1.dp),
            style = TextStyle(fontSize = 12.sp, lineHeight = 16.sp, fontWeight = FontWeight.Medium), color = Color.White,
        )
    }
}

@Composable
private fun ListCell(conversation: String, row: SharedMediaItem, open: (SharedMediaItem, ImageBitmap?) -> Unit) {
    val context = LocalContext.current
    val c = Promtuz.colors
    val record by rememberRecord(conversation, row)
    val media = record
    val date = DateUtils.formatDateTime(context, row.ms, DateUtils.FORMAT_SHOW_DATE or DateUtils.FORMAT_ABBREV_MONTH)
    if (row.tab() == SharedTab.Voice && media?.blob != null) {
        Column(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 10.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Text(listOf(row.senderName, date).filter { it.isNotBlank() }.joinToString(" · "), style = RowSubtitleStyle, color = c.muted)
            VoiceBlock(MessageContent.Voice(row.dispatchId.toHex(), row.mime, media.durationMs.toInt(), media.thumb ?: byteArrayOf(), media.blob!!),
                c.text, c.card)
        }
        return
    }
    SettingsRow(
        title = row.name.ifBlank { mediaLabel(row.kind.toInt()) },
        subtitle = listOfNotNull(row.size.toLong().takeIf { it > 0 }?.let { Formatter.formatShortFileSize(context, it) }, date)
            .joinToString(" · "),
        leadingSize = 40.dp,
        leading = {
            Box(Modifier.size(40.dp).clip(RoundedCornerShape(10.dp)).background(c.cardHigh), Alignment.Center) {
                FileGlyph(row.mime, c.text, c.accent)
            }
        },
        onClick = { open(row, null) },
    )
}

private class DownloadRequested : Exception()

private suspend fun openShared(context: Context, conversation: String, name: String, row: SharedMediaItem, thumb: ImageBitmap?) {
    val did = row.dispatchId.toHex()
    val media = CoreBridge.getMessageMedia(conversation.fromHex(), row.dispatchId) ?: error("Attachment unavailable")
    val location = MessageLocation(conversation, name, did, row.ms)
    val title = row.senderName.ifBlank { name }
    if (media.kind.toInt() == 1) {
        val bytes = media.blob ?: error("Image unavailable")
        MediaViewer.open(listOf(MediaItem(
            did, thumb, media.width.toInt().coerceAtLeast(1), media.height.toInt().coerceAtLeast(1),
            title = title, mime = media.mime, encoded = bytes,
            subtitle = dateAndTime(context, row.ms), byteSize = bytes.size.toLong(), message = location,
        )))
    } else if (media.kind.toInt() == 4 && media.sticker != null) {
        val sticker = media.sticker!!
        val bytes = CoreBridge.stickerImage(sticker)
        val poster = withContext(Dispatchers.Default) { decodeAvifThumb(bytes, 512) }
        MediaViewer.open(listOf(MediaItem(
            did, poster, sticker.width.toInt().coerceAtLeast(1), sticker.height.toInt().coerceAtLeast(1),
            title = title, mime = "image/avif", encoded = bytes, byteSize = bytes.size.toLong(), message = location,
        )))
    } else if (PrivateMediaAccess.path(conversation, media) != null) {
        val path = requireNotNull(PrivateMediaAccess.path(conversation, media))
        if (media.mime.startsWith("video/") || media.mime.startsWith("image/")) {
            val poster = thumb ?: media.thumb?.let { withContext(Dispatchers.Default) { decodeAvifThumb(it, 160) } }
            val (width, height) = if (media.width > 0u && media.height > 0u) media.width.toInt() to media.height.toInt()
                else (poster?.width ?: 1) to (poster?.height ?: 1)
            MediaViewer.open(listOf(MediaItem(did, poster, width, height,
                title = title, subtitle = dateAndTime(context, row.ms), mime = media.mime, filePath = path,
                shareName = media.name, byteSize = media.size.toLong(), message = location,
                videoPath = path.takeIf { media.mime.startsWith("video/") })))
        } else openMediaPath(context, path, media.mime)
    } else if (media.fileId != null) {
        CoreBridge.downloadAttachment(media.fileId!!)
        throw DownloadRequested()
    } else error("Attachment unavailable")
}

private const val Downloading = "Downloading…"
