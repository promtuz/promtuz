package com.promtuz.chat.ui.components

import android.text.format.Formatter
import com.promtuz.chat.ui.text.EmojiText
import com.promtuz.chat.ui.text.clock

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.InlineTextContent
import androidx.compose.foundation.text.appendInlineContent
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import com.promtuz.chat.utils.media.VoicePlayer
import androidx.compose.ui.text.Placeholder
import androidx.compose.ui.text.PlaceholderVerticalAlign
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.layout.layout
import androidx.compose.runtime.remember
import kotlin.math.roundToInt
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.ui.appearance.LocalChatAppearance
import com.promtuz.chat.ui.media.mediaOrigin
import com.promtuz.chat.ui.media.InlinePlayback
import com.promtuz.chat.ui.media.MediaItem
import com.promtuz.chat.ui.media.MediaFrame
import com.promtuz.chat.ui.media.MediaViewer
import com.promtuz.chat.ui.media.VideoSurface
import com.promtuz.chat.ui.media.rememberVideoPlayer
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.snapshotFlow
import com.promtuz.chat.ui.appearance.LocalChatColors
import androidx.compose.ui.res.painterResource
import com.promtuz.chat.utils.media.EncodedImage
import com.promtuz.chat.utils.media.StickerImage

private val AlbumGap = 2.dp

/** Sized from the stored dimensions, so an undecoded bitmap keeps the row's height. */
@Composable
fun ImageBlock(
    image: MessageContent.Image, textColor: Color, fontScale: Float, metaLabel: String,
    outgoing: Boolean = false,
    originKey: String? = null,
    onOpen: (() -> Unit)? = null,
) {
    val ratio = if (image.width > 0 && image.height > 0) image.width.toFloat() / image.height else 1f
    val corner = LocalChatAppearance.current.bubble.cornerRadius.dp
    val frame = remember(originKey) { MediaFrame() }
    Column {
        // No clip of its own: the bubble's shape does the rounding.
        Box(
            Modifier
                .mediaSize(ratio)
                .then(if (originKey != null) Modifier.mediaOrigin(originKey, corner) { frame.image } else Modifier)
                .then(if (onOpen != null) Modifier.tapOnly(onOpen) else Modifier)
                .background(textColor.copy(alpha = 0.10f)),
        ) {
            EncodedImage(
                bytes = image.encoded, poster = image.bitmap, contentDescription = null,
                modifier = Modifier.fillMaxSize(), contentScale = ContentScale.Crop,
                maxEdge = 2048, animate = MediaViewer.session == null,
                onFrameChanged = { frame.image = it },
            )
        }
        if (image.caption.isNotEmpty()) {
            Caption(image.caption, textColor, fontScale, metaLabel, outgoing, inset = true)
        }
    }
}

@Composable
fun AlbumBlock(
    album: MessageContent.Album, textColor: Color, fontScale: Float, metaLabel: String,
    outgoing: Boolean = false,
    onOpen: ((String) -> Unit)? = null,
    onDownload: ((String) -> Unit)? = null,
    onOpenFile: ((String) -> Unit)? = null,
) {
    val ratios = album.items.map { item ->
        when (val content = item.content) {
            is MessageContent.Image -> if (content.width > 0 && content.height > 0) content.width.toFloat() / content.height else 1f
            is MessageContent.Attachment -> content.thumb?.let { it.width.toFloat() / it.height } ?: 1f
            else -> 1f
        }
    }
    val cells = remember(ratios) { albumLayout(ratios) }
    Column {
        Layout(
            content = {
                album.items.forEach { item ->
                    val content = item.content
                    val frame = remember(item.dispatchIdHex) { MediaFrame() }
                    val open: (() -> Unit)? = when (content) {
                        is MessageContent.Image -> onOpen?.let { { it(item.dispatchIdHex) } }
                        is MessageContent.Attachment -> if (content.transferState == 2 && content.localPath != null) {
                            if (content.isMediaTile) onOpen?.let { { it(item.dispatchIdHex) } }
                            else onOpenFile?.let { { it(content.localPath) } }
                        } else null
                        else -> null
                    }
                    Box(
                        Modifier
                            .mediaOrigin(item.dispatchIdHex, 0.dp) { frame.image }
                            .then(if (open != null) Modifier.tapOnly(open) else Modifier)
                            .background(textColor.copy(alpha = 0.10f)),
                    ) {
                        when (content) {
                            is MessageContent.Image -> EncodedImage(
                                bytes = content.encoded, poster = content.bitmap, contentDescription = null,
                                modifier = Modifier.fillMaxSize(), contentScale = ContentScale.Crop,
                                maxEdge = 2048, animate = MediaViewer.session == null,
                                onFrameChanged = { frame.image = it },
                            )
                            is MessageContent.Attachment -> MediaTileContent(
                                content, textColor, outgoing, item.dispatchIdHex, onDownload,
                                onFrameChanged = { frame.image = it },
                            )
                            else -> Unit
                        }
                    }
                }
            },
        ) { measurables, constraints ->
            val width = constraints.maxWidth
            val maxHeight = width * ALBUM_HEIGHT_RATIO
            // Normalise: a layout that leaves a strip unused spans out to the full width.
            val spanW = cells.maxOf { it.x + it.w }.coerceAtLeast(0.01f)
            val spanH = cells.maxOf { it.y + it.h }
            // Gaps sit only between cells; outer edges run to the bubble's edge.
            val half = (AlbumGap / 2).roundToPx()
            val placed = measurables.mapIndexed { i, m ->
                val c = cells[i]
                val left = (c.x / spanW * width).roundToInt() + if (c.x > 0.001f) half else 0
                val top = (c.y * maxHeight).roundToInt() + if (c.y > 0.001f) half else 0
                val right = ((c.x + c.w) / spanW * width).roundToInt() - if (c.x + c.w < spanW - 0.001f) half else 0
                val bottom = ((c.y + c.h) * maxHeight).roundToInt() - if (c.y + c.h < spanH - 0.001f) half else 0
                m.measure(Constraints.fixed((right - left).coerceAtLeast(1), (bottom - top).coerceAtLeast(1))) to IntOffset(left, top)
            }
            val height = placed.maxOf { it.second.y + it.first.height }
            layout(width, height) { placed.forEach { (p, at) -> p.place(at) } }
        }
        if (album.caption.isNotEmpty()) {
            Caption(album.caption, textColor, fontScale, metaLabel, outgoing, inset = true)
        }
    }
}

private fun Modifier.mediaSize(ratio: Float) = layout { measurable, constraints ->
    val maxW = constraints.maxWidth
    val maxH = MediaMaxHeight.roundToPx()
    val minW = (maxW * 0.45f).roundToInt()
    var w = minOf(maxW.toFloat(), maxH * ratio).roundToInt()
    var h = (w / ratio).roundToInt()
    if (w < minW) { w = minW; h = minOf(maxH, (w / ratio).roundToInt()) }
    if (h > maxH) h = maxH
    val placeable = measurable.measure(Constraints.fixed(w, h))
    layout(w, h) { placeable.place(0, 0) }
}

private val MediaMaxHeight = 360.dp

@Composable
fun AttachmentBlock(
    att: MessageContent.Attachment,
    textColor: Color,
    fontScale: Float,
    metaLabel: String,
    peerName: String,
    outgoing: Boolean,
    onDownload: ((String) -> Unit)?,
    onOpen: ((String) -> Unit)?,
) {
    val size = Formatter.formatShortFileSize(LocalContext.current, att.size)
    // Outgoing shows plain size: retry and waiting states are receiver-side.
    val subtitle = if (outgoing) size else when (att.transferState) {
        1 -> if (att.transferTotal > 0) "$size · ${att.transferHave * 100 / att.transferTotal}%" else size
        3 -> "Tap to retry"
        4 -> if (peerName.isNotBlank()) "Waiting for $peerName…" else "Waiting…"
        5 -> "Connecting…"
        else -> size
    }
    Column {
        Row(
            Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(12.dp))
                .background(textColor.copy(alpha = 0.06f))
                .padding(8.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Box(
                Modifier.size(40.dp).clip(RoundedCornerShape(8.dp)).background(textColor.copy(alpha = 0.10f)),
                Alignment.Center,
            ) {
                att.thumb?.let { Image(it, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop) }
                    ?: FileGlyph(att.mime, textColor, LocalChatColors.current.accent)
            }
            Column(Modifier.weight(1f)) {
                Text(
                    att.name,
                    style = MaterialTheme.typography.bodyMedium,
                    color = textColor,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(subtitle, style = MaterialTheme.typography.labelSmall, color = textColor.copy(alpha = 0.6f))
            }
            TransferAffordance(att, textColor, outgoing, onDownload, onOpen)
        }
        Caption(att.caption, textColor, fontScale, metaLabel, outgoing, inset = false)
    }
}

val MessageContent.Attachment.isMediaTile: Boolean
    get() = thumb != null && (mime.startsWith("image/") || mime.startsWith("video/"))

@Composable
fun MediaTileBlock(
    att: MessageContent.Attachment, textColor: Color, fontScale: Float, metaLabel: String,
    outgoing: Boolean, originKey: String?, onDownload: ((String) -> Unit)?, onOpen: (() -> Unit)?,
) {
    val thumb = att.thumb ?: return
    val ratio = thumb.width.toFloat() / thumb.height
    val corner = LocalChatAppearance.current.bubble.cornerRadius.dp
    val ready = att.transferState == 2 && att.localPath != null
    val frame = remember(originKey) { MediaFrame() }
    Column {
        Box(
            Modifier
                .mediaSize(ratio)
                .then(if (originKey != null) Modifier.mediaOrigin(originKey, corner) { frame.image } else Modifier)
                .then(if (ready && onOpen != null) Modifier.tapOnly(onOpen) else Modifier)
                .background(textColor.copy(alpha = 0.10f)),
        ) {
            MediaTileContent(att, textColor, outgoing, originKey, onDownload, onFrameChanged = { frame.image = it })
        }
        if (att.caption.isNotEmpty()) Caption(att.caption, textColor, fontScale, metaLabel, outgoing, inset = true)
    }
}

/** The same download, progress and playback controls serve both a lone tile and an album cell. */
@Composable
private fun BoxScope.MediaTileContent(
    att: MessageContent.Attachment, textColor: Color, outgoing: Boolean,
    originKey: String?, onDownload: ((String) -> Unit)?,
    onFrameChanged: ((ImageBitmap?) -> Unit)? = null,
) {
    val thumb = att.thumb
    val video = att.mime.startsWith("video/") && thumb != null
    val ready = att.transferState == 2 && att.localPath != null
    // Until the bytes land only the ring is a control, so a tap never lights the whole picture.
    val download: (() -> Unit)? = when {
        ready || outgoing -> null
        att.transferState == 1 || att.transferState == 4 -> null
        else -> onDownload?.let { { it(att.fileIdHex) } }
    }
    if (thumb != null) EncodedImage(
        filePath = att.localPath.takeIf { ready && att.mime.startsWith("image/") }, poster = thumb, contentDescription = null,
        modifier = Modifier.fillMaxSize(), contentScale = ContentScale.Crop,
        maxEdge = 2048, animate = MediaViewer.session == null,
        onFrameChanged = onFrameChanged,
    ) else if (ready) Column(
        Modifier.align(Alignment.Center).padding(12.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        FileGlyph(att.mime, textColor, LocalChatColors.current.accent)
        Text(att.name, color = textColor, style = MaterialTheme.typography.labelSmall, maxLines = 2, overflow = TextOverflow.Ellipsis)
    }
    val playingThumb = thumb?.takeIf { video && ready && originKey != null && InlinePlayback.key == originKey }
    val playingHere = playingThumb != null
    if (playingThumb != null && originKey != null) {
        val player = rememberVideoPlayer(att.localPath ?: return, active = true)
        DisposableEffect(player) {
            InlinePlayback.attach(originKey, player)
            onDispose { if (InlinePlayback.key == originKey) InlinePlayback.stop() }
        }
        LaunchedEffect(player.ended) { if (player.ended) InlinePlayback.stop() }
        VideoSurface(player, MediaItem(originKey, playingThumb, playingThumb.width, playingThumb.height), Modifier.fillMaxSize())
    }
    if (!ready) Box(
        Modifier.align(Alignment.Center)
            .then(if (download != null) Modifier.pressScale(download) else Modifier)
            .size(44.dp).clip(RoundedCornerShape(22.dp))
            .background(Color.Black.copy(alpha = 0.45f)),
        contentAlignment = Alignment.Center,
    ) {
        if (outgoing || att.transferState == 1) {
            if (att.transferTotal > 0) CircularProgressIndicator(
                progress = { att.transferHave.toFloat() / att.transferTotal },
                modifier = Modifier.size(28.dp), color = Color.White, strokeWidth = 2.dp,
            ) else CircularProgressIndicator(Modifier.size(28.dp), color = Color.White, strokeWidth = 2.dp)
        } else Image(painterResource(R.drawable.ic_media_download), "Download", Modifier.size(24.dp))
    } else if (video && !playingHere) Image(
        painterResource(R.drawable.ic_media_play_badge), "Play",
        Modifier.align(Alignment.Center)
            .then(if (originKey != null) Modifier.pressScale({ InlinePlayback.play(originKey) }) else Modifier)
            .size(48.dp),
    )
    Text(
        if (att.transferState == 4) "Waiting…" else Formatter.formatShortFileSize(LocalContext.current, att.size),
        style = MaterialTheme.typography.labelSmall, color = Color.White,
        modifier = Modifier.align(Alignment.TopStart).padding(6.dp)
            .clip(RoundedCornerShape(8.dp)).background(Color.Black.copy(alpha = 0.45f))
            .padding(horizontal = 6.dp, vertical = 2.dp),
    )
}

/** With a [surface] the player is its own pill on the wallpaper; without one it sits inside a bubble. */
@Composable
fun VoiceBlock(voice: MessageContent.Voice, textColor: Color, surface: Color? = null) {
    val context = LocalContext.current
    val playback by VoicePlayer.state.collectAsState()
    val mine = playback?.takeIf { it.dispatchIdHex == voice.dispatchIdHex }
    val playing = mine?.playing == true
    val position = mine?.positionMs ?: 0
    val fraction = if (voice.durationMs > 0) (position.toFloat() / voice.durationMs).coerceIn(0f, 1f) else 0f
    val shown = if (mine != null) (voice.durationMs - position).coerceAtLeast(0) else voice.durationMs
    Row(
        Modifier
            .width(VoiceWidth)
            .clip(RoundedCornerShape(26.dp))
            .background(surface ?: textColor.copy(alpha = 0.06f))
            .padding(start = 6.dp, end = 14.dp, top = 6.dp, bottom = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Box(
            Modifier
                .size(40.dp)
                .clip(RoundedCornerShape(20.dp))
                .background(textColor.copy(alpha = if (surface != null) 0.18f else 0.10f))
                .clickable { VoicePlayer.toggle(context, voice.dispatchIdHex, voice.bytes, voice.mime) },
            Alignment.Center,
        ) {
            MorphIcon(
                if (playing) MorphGlyph.Pause else MorphGlyph.Play,
                if (playing) "Pause voice message" else "Play voice message",
                Modifier.size(18.dp),
                tint = textColor,
            )
        }
        Waveform(voice.waveform, fraction, textColor, Modifier.weight(1f).height(28.dp))
        Text(
            clock(shown + 500L),
            style = MaterialTheme.typography.labelMedium,
            color = textColor.copy(alpha = 0.8f),
        )
    }
}

private val VoiceWidth = 232.dp

/** Reserve the sticker's dimensions while downloading to keep chat layout stable. */
@Composable
fun StickerBlock(sticker: MessageContent.Sticker, textColor: Color) {
    val ref = sticker.sticker
    val ratio = (if (ref.width > 0 && ref.height > 0) ref.width.toFloat() / ref.height else 1f)
        .coerceIn(0.5f, 2f)
    val width = if (ratio >= 1f) StickerBox else StickerBox * ratio
    val height = width / ratio
    Box(
        Modifier
            .size(width, height)
            .clip(RoundedCornerShape(10.dp)),
        Alignment.Center,
    ) {
        StickerImage(
            ref, null, Modifier.fillMaxSize(),
            contentScale = ContentScale.Fit, animate = MediaViewer.session == null,
            placeholderColor = textColor.copy(alpha = 0.08f),
        )
    }
}

private val StickerBox = 160.dp

/** Bars from 0..255 loudness samples; a missing waveform draws as a flat line. */
@Composable
private fun Waveform(samples: ByteArray, lit: Float, color: Color, modifier: Modifier) {
    Canvas(modifier) {
        val n = if (samples.isEmpty()) 32 else samples.size
        val step = size.width / n
        val stroke = (step * 0.55f).coerceIn(2f, 6f)
        for (i in 0 until n) {
            val v = if (samples.isEmpty()) 0.15f else (samples[i].toInt() and 0xff) / 255f
            val h = (size.height * (0.15f + 0.85f * v)).coerceAtLeast(stroke)
            val x = step * i + step / 2
            val played = (i + 0.5f) / n <= lit
            drawLine(
                color = color.copy(alpha = if (played) 0.95f else 0.35f),
                start = Offset(x, (size.height - h) / 2),
                end = Offset(x, (size.height + h) / 2),
                strokeWidth = stroke,
                cap = StrokeCap.Round,
            )
        }
    }
}

@Composable
private fun TransferAffordance(
    att: MessageContent.Attachment,
    textColor: Color,
    outgoing: Boolean,
    onDownload: ((String) -> Unit)?,
    onOpen: ((String) -> Unit)?,
) {
    // An outgoing file is still sending until state 2; there is nothing to download.
    if (outgoing && att.transferState != 2) {
        CircularProgressIndicator(Modifier.size(20.dp), color = textColor, strokeWidth = 2.dp)
        return
    }
    when (att.transferState) {
        // A tap re-drives download(), which no-ops a live pull and resumes a stalled one.
        1, 5 -> {
            val ring = Modifier.size(26.dp).clickable { onDownload?.invoke(att.fileIdHex) }
            if (att.transferState == 1 && att.transferTotal > 0)
                CircularProgressIndicator(
                    progress = { att.transferHave.toFloat() / att.transferTotal },
                    modifier = ring,
                    color = textColor,
                    strokeWidth = 2.dp,
                )
            else CircularProgressIndicator(ring, color = textColor, strokeWidth = 2.dp)
        }
        2 -> IconButton({ att.localPath?.let { onOpen?.invoke(it) } }) {
            MorphIcon(MorphGlyph.Check, null, Modifier.size(20.dp), tint = textColor)
        }
        else -> IconButton({ onDownload?.invoke(att.fileIdHex) }) {
            val tint = if (att.transferState == 3) MaterialTheme.colorScheme.error else textColor
            DrawableIcon(R.drawable.i_download, Modifier.size(20.dp), tint = tint)
        }
    }
}

/** Reserves the corner meta slot inline so the timestamp never lands on the last glyph. */
@Composable
private fun Caption(
    text: String, textColor: Color, fontScale: Float, metaLabel: String,
    outgoing: Boolean, inset: Boolean,
) {
    val style = if (text.isEmpty()) MaterialTheme.typography.labelSmall
    else MaterialTheme.typography.bodyLarge.let { it.copy(fontSize = it.fontSize * fontScale) }
    val density = LocalDensity.current
    val measurer = rememberTextMeasurer()
    val metaStyle = MaterialTheme.typography.labelSmall
    // Use the exact label and fixed status footprint rendered by MetaRow. The
    // measurer already caches; an outer remember would miss density/font changes.
    val labelSize = measurer.measure(metaLabel, metaStyle, maxLines = 1, softWrap = false).size
    val metaWidth = with(density) {
        val statusPx = if (outgoing) {
            BubbleStatusSize.roundToPx() + BubbleStatusGap.roundToPx()
        } else 0
        (labelSize.width + 8.dp.roundToPx() + statusPx).toSp()
    }
    val metaHeight = with(density) {
        maxOf(labelSize.height, if (outgoing) BubbleStatusSize.roundToPx() else 0).toSp()
    }

    val annotated = buildAnnotatedString {
        append(text)
        appendInlineContent("meta")
    }
    val inline = mapOf(
        "meta" to InlineTextContent(Placeholder(metaWidth, metaHeight, PlaceholderVerticalAlign.TextBottom)) {}
    )
    com.promtuz.chat.ui.text.MessageText(
        annotated,
        if (inset) Modifier.padding(start = BubblePadH, end = BubblePadH, top = 4.dp, bottom = BubblePadV)
        else Modifier.padding(top = 4.dp),
        style = style,
        color = textColor,
        inlineContent = inline,
    )
}

@Composable
fun FileGlyph(mime: String, textColor: Color, accent: Color, modifier: Modifier = Modifier) {
    val label = when {
        mime == "application/pdf" -> R.drawable.ic_file_label_pdf
        mime.startsWith("audio/") -> R.drawable.ic_file_label_audio
        mime == "application/vnd.android.package-archive" -> R.drawable.ic_file_label_apk
        mime.contains("zip") || mime.contains("compressed") || mime.contains("rar") || mime.contains("tar") -> R.drawable.ic_file_label_zip
        mime.contains("spreadsheet") || mime.contains("excel") || mime == "text/csv" -> R.drawable.ic_file_label_xls
        mime.contains("word") || mime.contains("document") || mime.startsWith("text/") -> R.drawable.ic_file_label_doc
        else -> R.drawable.ic_file_label_generic
    }
    Box(modifier.size(28.dp)) {
        DrawableIcon(R.drawable.ic_file_base, Modifier.fillMaxSize(), tint = textColor)
        DrawableIcon(R.drawable.ic_file_badge, Modifier.fillMaxSize(), tint = accent)
        DrawableIcon(label, Modifier.fillMaxSize(), tint = Color.White)
    }
}
