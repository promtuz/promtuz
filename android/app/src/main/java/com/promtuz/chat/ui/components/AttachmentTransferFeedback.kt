package com.promtuz.chat.ui.components

import android.media.MediaMetadataRetriever
import android.text.format.Formatter
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.expandVertically
import androidx.compose.animation.shrinkVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.clipRect
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalWindowInfo
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.ui.stage.ChatMotion
import com.promtuz.chat.ui.text.clock
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.core.AttachmentTransferRecord

internal class AttachmentFeedback(
    val state: Int,
    val outgoing: Boolean,
    val rows: List<AttachmentTransferRecord>,
    val download: (() -> Unit)?,
    val onOpen: (() -> Unit)?,
    val details: () -> Unit,
) {
    val uploading get() = outgoing && rows.any { it.active }
    val progress get() = rows.firstOrNull { it.active && !it.outgoing }
    val status get() = when (state) {
        1 -> "Downloading…"
        2 -> "Downloaded"
        3 -> "Couldn’t download · Tap to retry"
        4 -> "Waiting for a source…"
        5 -> "Connecting…"
        else -> "Not downloaded"
    }
}

@Composable
internal fun rememberAttachmentFeedback(
    att: MessageContent.Attachment, outgoing: Boolean, onDownload: ((String) -> Unit)?,
    onOpen: (() -> Unit)?,
): AttachmentFeedback {
    val activity by CoreBridge.attachmentTransfers.collectAsStateWithLifecycle()
    val rows = remember(activity, att.fileIdHex, outgoing) {
        activity.filter { it.outgoing == outgoing && it.fileId.toHex() == att.fileIdHex }
    }
    var requested by remember(att.fileIdHex) { mutableStateOf(false) }
    var details by remember(att.fileIdHex) { mutableStateOf(false) }
    LaunchedEffect(att.transferState, rows.any { it.active }) {
        if (att.transferState in setOf(1, 2, 4, 5) || rows.any { it.active }) requested = false
    }
    val state = if (requested && att.transferState != 2) 5 else att.transferState
    val download = onDownload?.let { request -> {
        if (state !in setOf(1, 2, 5)) {
            requested = true
            request(att.fileIdHex)
        }
    } }
    val feedback = AttachmentFeedback(state, outgoing, rows, download,
        onOpen.takeIf { att.localPath != null && att.transferState == 2 }) { details = true }
    if (details) AttachmentTransferSheet(att, feedback) { details = false }
    return feedback
}

@Composable
internal fun TransferControl(
    feedback: AttachmentFeedback, color: Color, modifier: Modifier = Modifier,
    ready: @Composable () -> Unit,
) {
    val state = if (feedback.outgoing) 2 else feedback.state
    val action = if (state in setOf(1, 4, 5)) feedback.details else feedback.download ?: feedback.details
    Box(modifier.size(48.dp), contentAlignment = Alignment.Center) {
        AnimatedContent(state, transitionSpec = {
            fadeIn(ChatMotion.spec()) togetherWith fadeOut(ChatMotion.spec())
        }, contentAlignment = Alignment.Center, label = "attachment action") { shown ->
            if (shown == 2) ready() else Box(
                Modifier.size(44.dp).pressScale(action)
                    .semantics { contentDescription = feedback.status },
                contentAlignment = Alignment.Center,
            ) {
                when (shown) {
                    1 -> {
                        val row = feedback.progress
                        if (row != null && row.total > 0uL) {
                            val fraction by animateFloatAsState(
                                (row.bytes.toFloat() / row.total.toFloat()).coerceIn(0f, 1f),
                                ChatMotion.spec(), label = "download progress",
                            )
                            CircularProgressIndicator(progress = { fraction }, Modifier.size(28.dp), color, strokeWidth = 2.dp)
                        } else CircularProgressIndicator(Modifier.size(28.dp), color, strokeWidth = 2.dp)
                    }
                    5 -> CircularProgressIndicator(Modifier.size(28.dp), color, strokeWidth = 2.dp)
                    4 -> DrawableIcon(R.drawable.oi_info, Modifier.size(24.dp), tint = color)
                    else -> DrawableIcon(R.drawable.ic_media_download, Modifier.size(24.dp),
                        tint = if (shown == 3) MaterialTheme.colorScheme.error else color)
                }
            }
        }
    }
}

@Composable
internal fun TransferStatus(
    att: MessageContent.Attachment, feedback: AttachmentFeedback, color: Color,
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    val duration by produceState<Long?>(null, att.localPath, att.mime) {
        if (!att.mime.startsWith("video/") || att.localPath == null) return@produceState
        value = withContext(Dispatchers.IO) {
            runCatching {
                val metadata = MediaMetadataRetriever()
                try {
                    metadata.setDataSource(att.localPath)
                    metadata.extractMetadata(MediaMetadataRetriever.METADATA_KEY_DURATION)?.toLongOrNull()
                } finally { metadata.release() }
            }.getOrNull()
        }
    }
    val row = feedback.progress
    val label = when {
        duration != null -> duration?.let(::clock).orEmpty()
        att.mime.startsWith("video/") -> "–:––"
        att.mime.startsWith("image/") -> "Photo"
        else -> "File"
    }
    val transfer = if (!feedback.outgoing && feedback.state != 2) {
        when {
            feedback.state == 1 && row != null -> buildString {
                append(Formatter.formatShortFileSize(context, row.bytes.toLong()))
                append(" / ")
                append(Formatter.formatShortFileSize(context, att.size))
                if (row.bytesPerSecond > 0uL) {
                    append(" · ")
                    append(Formatter.formatShortFileSize(context, row.bytesPerSecond.toLong()))
                    append("/s")
                }
            }
            feedback.state != 0 -> feedback.status
            else -> null
        }
    } else null
    Row(modifier.animateContentSize(ChatMotion.spec()).clickable(onClick = feedback.details)
        .padding(horizontal = 6.dp, vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        if (feedback.uploading) {
            val motion = rememberInfiniteTransition(label = "sending arrow")
            val rise by motion.animateFloat(2f, -2f,
                infiniteRepeatable(tween(650, easing = ChatMotion.Easing), RepeatMode.Reverse), label = "sending rise")
            Image(painterResource(R.drawable.ic_media_download), "Sending attachment",
                Modifier.size(14.dp).graphicsLayer { rotationZ = 180f; translationY = rise.dp.toPx() },
                colorFilter = androidx.compose.ui.graphics.ColorFilter.tint(color))
        }
        Text(listOfNotNull(label, transfer).joinToString(" · "), color = color, style = MaterialTheme.typography.labelSmall,
            maxLines = 2, overflow = TextOverflow.Ellipsis)
    }
}

@Composable
private fun AttachmentTransferSheet(
    att: MessageContent.Attachment, feedback: AttachmentFeedback, onDismiss: () -> Unit,
) {
    var rows by remember(att.fileIdHex) { mutableStateOf<List<AttachmentTransferRecord>?>(null) }
    var failed by remember { mutableStateOf(false) }
    var retry by remember { mutableIntStateOf(0) }
    val activity by CoreBridge.attachmentTransfers.collectAsStateWithLifecycle()
    LaunchedEffect(activity, att.transferState, retry) {
        try {
            rows = CoreBridge.attachmentTransferDetails(att.fileIdHex.fromHex(), feedback.outgoing)
            failed = false
        } catch (cancel: CancellationException) { throw cancel }
        catch (_: Exception) { failed = true }
    }
    val context = LocalContext.current
    val colors = MaterialTheme.colorScheme
    val maxHeight = with(LocalDensity.current) { LocalWindowInfo.current.containerSize.height.toDp() } * 0.8f
    val sheetState = rememberBottomSheetState(
        initialValue = SheetValue.Hidden,
        enabledValues = setOf(SheetValue.Hidden, SheetValue.Expanded),
    )
    val scope = rememberCoroutineScope()
    var expanded by remember(att.fileIdHex) { mutableStateOf(false) }
    val single = rows?.singleOrNull()
    val technical = rows.orEmpty().filter { it.groups.isNotEmpty() }
    val download = feedback.download
    val open = feedback.onOpen
    AppBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = sheetState,
    ) {
        LazyColumn(Modifier.fillMaxWidth().heightIn(max = maxHeight),
            contentPadding = PaddingValues(start = 24.dp, end = 24.dp, top = 8.dp, bottom = 24.dp),
            verticalArrangement = Arrangement.spacedBy(24.dp),
        ) {
            item {
                Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                    Text("Transfer details", Modifier.weight(1f), style = MaterialTheme.typography.titleLarge)
                    IconButton({ scope.launch { sheetState.hide(); onDismiss() } }) {
                        MorphIcon(MorphGlyph.Close, "Close", Modifier.size(20.dp), tint = colors.onSurfaceVariant)
                    }
                }
            }
            item {
                Row(verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(16.dp)) {
                    Box(Modifier.size(56.dp).clip(MaterialTheme.shapes.medium)
                        .background(colors.surfaceContainerHighest), contentAlignment = Alignment.Center) {
                        att.thumb?.let { Image(it, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop) }
                            ?: FileGlyph(att.mime, colors.onSurfaceVariant, colors.primary)
                    }
                    Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                        Text(att.name, style = MaterialTheme.typography.titleMedium,
                            maxLines = 2, overflow = TextOverflow.Ellipsis)
                        val source = single?.peerName?.takeIf { it.isNotBlank() }
                        Text(listOfNotNull(Formatter.formatShortFileSize(context, att.size), source?.let {
                            if (feedback.outgoing) "To $it" else "Source: $it"
                        }).joinToString(" · "),
                            style = MaterialTheme.typography.bodyMedium, color = colors.onSurfaceVariant)
                    }
                }
            }
            if (failed) item {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text("Couldn’t load transfer details", Modifier.weight(1f))
                    TextButton({ retry++ }) { Text("Retry") }
                }
            }
            if (!feedback.outgoing) item {
                TransferProgress(feedback, single)
            } else if (rows.isNullOrEmpty()) item {
                if (rows == null && !failed) LinearProgressIndicator(Modifier.fillMaxWidth())
                else Text("Not currently sending", style = MaterialTheme.typography.bodyMedium,
                    color = colors.onSurfaceVariant)
            } else items(rows.orEmpty(), key = { it.peer.toHex() }) { row ->
                Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                    if (single == null) Text("To ${row.peerName}", style = MaterialTheme.typography.titleSmall)
                    TransferProgress(feedback, row)
                }
            }
            if (download != null && !feedback.outgoing && feedback.state in setOf(0, 3, 4)) item {
                Button(download, Modifier.fillMaxWidth()) {
                    Text(if (feedback.state == 3) "Retry download" else "Download")
                }
            }
            if (open != null) item {
                Button({ scope.launch { sheetState.hide(); onDismiss(); open() } }, Modifier.fillMaxWidth()) {
                    Text(if (att.mime.startsWith("video/")) "Open video" else
                        if (att.mime.startsWith("image/")) "Open image" else "Open file")
                }
            }
            if (technical.isNotEmpty()) item {
                Column {
                    HorizontalDivider(color = colors.outlineVariant)
                    Row(Modifier.fillMaxWidth().clickable { expanded = !expanded }
                        .semantics { stateDescription = if (expanded) "Expanded" else "Collapsed" }
                        .padding(vertical = 16.dp), verticalAlignment = Alignment.CenterVertically) {
                        Text("File parts", Modifier.weight(1f), style = MaterialTheme.typography.titleSmall)
                        MorphIcon(if (expanded) MorphGlyph.ChevronUp else MorphGlyph.ChevronDown,
                            null, Modifier.size(16.dp), tint = colors.onSurfaceVariant)
                    }
                    AnimatedVisibility(expanded,
                        enter = expandVertically(ChatMotion.spec()) + fadeIn(ChatMotion.spec()),
                        exit = shrinkVertically(ChatMotion.spec()) + fadeOut(ChatMotion.spec())) {
                        Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
                            if (!feedback.outgoing && att.transferTotal > 0) Row(Modifier.fillMaxWidth()) {
                                Text("Verified chunks", Modifier.weight(1f), style = MaterialTheme.typography.bodySmall,
                                    color = colors.onSurfaceVariant)
                                Text("${att.transferHave} / ${att.transferTotal}", style = MaterialTheme.typography.bodySmall)
                            }
                            technical.forEach { row ->
                                if (technical.size > 1) Text("To ${row.peerName}", style = MaterialTheme.typography.labelLarge)
                                ChunkMap(row)
                            }
                            if (feedback.outgoing) Text("Only this device’s activity is shown. Sent bytes aren’t a download receipt.",
                                style = MaterialTheme.typography.bodySmall, color = colors.onSurfaceVariant)
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun TransferProgress(feedback: AttachmentFeedback, row: AttachmentTransferRecord?) {
    val context = LocalContext.current
    val colors = MaterialTheme.colorScheme
    val complete = !feedback.outgoing && feedback.state == 2
    val fraction = if (complete) 1.0 else row?.takeIf { it.total > 0uL }
        ?.let { (it.bytes.toDouble() / it.total.toDouble()).coerceIn(0.0, 1.0) }
    val status = if (feedback.outgoing) when {
        row?.active == true && row.bytes >= row.total -> "Finishing…"
        row?.active == true -> "Sending…"
        row != null && row.bytes >= row.total -> "Sent to connection"
        else -> "Stopped"
    } else when (feedback.state) {
        1 -> if (fraction == 1.0) "Finishing…" else "Downloading…"
        2 -> "Downloaded"
        3 -> "Download failed"
        4 -> "Waiting for a source…"
        5 -> "Connecting…"
        else -> "Ready to download"
    }
    Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Text(status, Modifier.weight(1f).semantics { liveRegion = LiveRegionMode.Polite },
                style = MaterialTheme.typography.titleSmall)
            if (fraction != null) Text(java.text.NumberFormat.getPercentInstance().apply {
                roundingMode = java.math.RoundingMode.DOWN
            }.format(fraction),
                style = MaterialTheme.typography.labelMedium.copy(fontFeatureSettings = "tnum"),
                color = colors.onSurfaceVariant)
        }
        if (fraction == null && feedback.state in setOf(1, 5)) LinearProgressIndicator(Modifier.fillMaxWidth())
        else {
            val progress by animateFloatAsState(fraction?.toFloat() ?: 0f, ChatMotion.spec(), label = "transfer detail progress")
            LinearProgressIndicator(progress = { progress }, Modifier.fillMaxWidth())
        }
        if (complete || row != null) FlowRow(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween,
            verticalArrangement = Arrangement.spacedBy(4.dp)) {
            val bytes = when {
                complete -> "Available on this device"
                row != null -> "${Formatter.formatShortFileSize(context, row.bytes.toLong())} of ${Formatter.formatShortFileSize(context, row.total.toLong())}"
                else -> null
            }
            bytes?.let { Text(it, style = MaterialTheme.typography.bodySmall,
                color = colors.onSurfaceVariant) }
            if (!complete && row?.active == true && row.bytesPerSecond > 0uL) {
                Text(listOfNotNull("${Formatter.formatShortFileSize(context, row.bytesPerSecond.toLong())}/s",
                    row.remainingSeconds?.let { "~${clock(it.toLong() * 1000)} left" }).joinToString(" · "),
                    style = MaterialTheme.typography.bodySmall.copy(fontFeatureSettings = "tnum"),
                    color = colors.onSurfaceVariant)
            }
        }
    }
}

@Composable
private fun ChunkMap(row: AttachmentTransferRecord) {
    val colors = MaterialTheme.colorScheme
    val description = if (row.outgoing) "File parts sent to the connection" else "Verified file parts"
    Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
        BoxWithConstraints {
            val columns = minOf(row.groups.size, (maxWidth / 12.dp).toInt().coerceAtLeast(1))
            val height = ((row.groups.size + columns - 1) / columns * 12).dp
            Canvas(Modifier.fillMaxWidth().height(height).semantics {
                contentDescription = description
                stateDescription = "${row.groups.sumOf { it.completed.toLong() }} of " +
                    "${row.groups.sumOf { it.total.toLong() }} parts ${if (row.outgoing) "sent" else "verified"}"
            }) {
                val step = size.width / columns
                row.groups.forEachIndexed { index, group ->
                    val left = index % columns * step
                    val top = (index / columns * 12).dp.toPx()
                    val cell = androidx.compose.ui.geometry.Size((step - 3.dp.toPx()).coerceAtLeast(0f), 9.dp.toPx())
                    val origin = androidx.compose.ui.geometry.Offset(left, top)
                    val radius = androidx.compose.ui.geometry.CornerRadius(2.dp.toPx())
                    drawRoundRect(colors.outlineVariant, origin, cell, radius)
                    val fraction = if (group.total == 0u) 0f else
                        (group.completed.toFloat() / group.total.toFloat()).coerceIn(0f, 1f)
                    clipRect(left, top, left + cell.width * fraction, top + cell.height) {
                        drawRoundRect(colors.primary, origin, cell, radius)
                    }
                }
            }
        }
        FlowRow(horizontalArrangement = Arrangement.spacedBy(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            listOf((if (row.outgoing) "Sent" else "Verified") to 1f, "Partial" to 0.5f, "Missing" to 0f).forEach { (label, fraction) ->
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    Box(Modifier.size(8.dp).clip(RoundedCornerShape(2.dp)).background(colors.outlineVariant)) {
                        Box(Modifier.fillMaxHeight().fillMaxWidth(fraction).background(colors.primary))
                    }
                    Text(label, style = MaterialTheme.typography.bodySmall, color = colors.onSurfaceVariant)
                }
            }
        }
        Text("Blocks summarize neighboring file parts.", style = MaterialTheme.typography.bodySmall,
            color = colors.onSurfaceVariant)
    }
}
