package com.promtuz.chat.ui.components

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
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
import com.promtuz.chat.ui.media.VideoSurface
import com.promtuz.chat.ui.media.rememberVideoPlayer
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.snapshotFlow
import com.promtuz.chat.ui.appearance.LocalChatColors
import androidx.compose.ui.res.painterResource
import com.promtuz.chat.utils.media.rememberStickerBitmap
import java.util.Locale

private val MediaRadius = RoundedCornerShape(14.dp)

/** Hairline between album cells — enough to read as separate photos, not as a grid. */
private val AlbumGap = 2.dp

/**
 * Inline image. The box reserves the image's aspect-ratio footprint from
 * [width]/[height] so the windowed stage keeps a stable row height whether or
 * not [MessageContent.Image.bitmap] decoded (null on API levels without AVIF) —
 * a null bitmap shows a muted stand-in of the same size, never a collapse.
 */
@Composable
fun ImageBlock(
    image: MessageContent.Image, textColor: Color, fontScale: Float, metaLabel: String,
    outgoing: Boolean = false,
    originKey: String? = null,
    onOpen: (() -> Unit)? = null,
) {
    val ratio = if (image.width > 0 && image.height > 0) image.width.toFloat() / image.height else 1f
    val corner = LocalChatAppearance.current.bubble.cornerRadius.dp
    Column {
        // No clip of its own: the picture runs to the bubble's edge and the
        // bubble's shape does the rounding, so there's one outline, not two.
        Box(
            Modifier
                .mediaSize(ratio)
                .then(if (originKey != null) Modifier.mediaOrigin(originKey, corner) else Modifier)
                .then(if (onOpen != null) Modifier.tapOnly(onOpen) else Modifier)
                .background(textColor.copy(alpha = 0.10f)),
        ) {
            image.bitmap?.let {
                Image(it, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop)
            }
        }
        if (image.caption.isNotEmpty()) {
            Caption(image.caption, textColor, fontScale, metaLabel, outgoing, inset = true)
        }
    }
}

/**
 * An album: the cells come from [albumLayout], sized by each picture's proportions, and the whole thing
 * fills the bubble's width. Every cell is its own message and its own tap.
 */
@Composable
fun AlbumBlock(
    album: MessageContent.Album, textColor: Color, fontScale: Float, metaLabel: String,
    outgoing: Boolean = false,
    onOpen: ((String) -> Unit)? = null,
) {
    val ratios = album.items.map { item ->
        (item.content as? MessageContent.Image)?.let { if (it.width > 0 && it.height > 0) it.width.toFloat() / it.height else 1f } ?: 1f
    }
    val cells = remember(ratios) { albumLayout(ratios) }
    Column {
        Layout(
            content = {
                album.items.forEach { item ->
                    Box(
                        Modifier
                            .mediaOrigin(item.dispatchIdHex, 0.dp)
                            .then(if (onOpen != null) Modifier.tapOnly { onOpen(item.dispatchIdHex) } else Modifier)
                            .background(textColor.copy(alpha = 0.10f)),
                    ) {
                        albumBitmap(item.content)?.let {
                            Image(it, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop)
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
            // Gaps sit only between cells; the outer edges run to the bubble's own edge so
            // its rounding is the only outline the album has.
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

/**
 * How big a picture is in a bubble. It takes the bubble's full width until that
 * would make it taller than [MediaMaxHeight]; past that the width gives way, so a
 * tall photo becomes a narrower bubble rather than a cropped one. A sliver still
 * keeps a minimum width, where cropping is the lesser evil.
 */
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

private fun albumBitmap(content: MessageContent) = when (content) {
    is MessageContent.Image -> content.bitmap
    is MessageContent.Attachment -> content.thumb
    else -> null
}

/**
 * A P2P file card: thumb (or a mime glyph), name + size, and a transfer
 * affordance driven by [MessageContent.Attachment.transferState] — tap to
 * download when idle/failed/held, a determinate ring while pulling, open when
 * done. Progress arrives by reactive re-read; this stays a pure renderer.
 */
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
    // Plain-language transfer line — no P2P/relay jargon. "Waiting" = the sender's
    // offline. Outgoing shows plain size: retry/waiting states are receiver-side.
    val subtitle = if (outgoing) formatBytes(att.size) else when (att.transferState) {
        1 -> if (att.transferTotal > 0)
            "${formatBytes(att.size)} · ${att.transferHave * 100 / att.transferTotal}%"
        else formatBytes(att.size)
        3 -> "Tap to retry"
        4 -> if (peerName.isNotBlank()) "Waiting for $peerName…" else "Waiting…"
        5 -> "Connecting…"
        else -> formatBytes(att.size)
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

/** A picture or video sent as a file is drawn as a tile once it carries a poster. */
val MessageContent.Attachment.isMediaTile: Boolean
    get() = thumb != null && (mime.startsWith("image/") || mime.startsWith("video/"))

/**
 * A picture or video attachment as a media tile: poster at its own aspect, the
 * transfer ring over it until the bytes land, a play badge and size pill for a
 * video. Tapping a finished one opens the viewer; tapping anything else drives
 * the download, the same as the file card's ring.
 */
@Composable
fun MediaTileBlock(
    att: MessageContent.Attachment, textColor: Color, fontScale: Float, metaLabel: String,
    outgoing: Boolean, originKey: String?, onDownload: ((String) -> Unit)?, onOpen: (() -> Unit)?,
) {
    val thumb = att.thumb ?: return
    val ratio = thumb.width.toFloat() / thumb.height
    val corner = LocalChatAppearance.current.bubble.cornerRadius.dp
    val video = att.mime.startsWith("video/")
    val ready = att.transferState == 2 && att.localPath != null
    // The tile opens once the bytes are here; before that only the ring is a control,
    // and it is its own control, so a tap on it never lights the whole picture.
    val download: (() -> Unit)? = when {
        ready || outgoing -> null
        att.transferState == 1 || att.transferState == 4 -> null
        else -> onDownload?.let { { it(att.fileIdHex) } }
    }
    Column {
        Box(
            Modifier
                .mediaSize(ratio)
                .then(if (originKey != null) Modifier.mediaOrigin(originKey, corner) else Modifier)
                .then(if (ready && onOpen != null) Modifier.tapOnly(onOpen) else Modifier)
                .background(textColor.copy(alpha = 0.10f)),
        ) {
            Image(thumb, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop)
            // A clip plays in its own bubble; tapping the playing picture carries it into the viewer.
            val playingHere = video && ready && originKey != null && InlinePlayback.key == originKey
            if (playingHere) {
                val player = rememberVideoPlayer(att.localPath!!, active = true)
                DisposableEffect(player) {
                    InlinePlayback.attach(originKey, player)
                    onDispose { if (InlinePlayback.key == originKey) InlinePlayback.stop() }
                }
                LaunchedEffect(player.ended) { if (player.ended) InlinePlayback.stop() }
                VideoSurface(player, MediaItem(originKey, thumb, thumb.width, thumb.height), Modifier.fillMaxSize())
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
                if (att.transferState == 4) "Waiting…" else formatBytes(att.size),
                style = MaterialTheme.typography.labelSmall, color = Color.White,
                modifier = Modifier.align(Alignment.TopStart).padding(6.dp)
                    .clip(RoundedCornerShape(8.dp)).background(Color.Black.copy(alpha = 0.45f))
                    .padding(horizontal = 6.dp, vertical = 2.dp),
            )
        }
        if (att.caption.isNotEmpty()) Caption(att.caption, textColor, fontScale, metaLabel, outgoing, inset = true)
    }
}

/**
 * A voice note: play/pause, the sender's waveform with the played part lit,
 * and the clock — remaining while it plays, total otherwise.
 *
 * With a [surface] the player is its own pill on the wallpaper, in the
 * bubble's colour; without one it sits inside a bubble (a reply) as a
 * quieter inset card.
 */
@Composable
fun VoiceBlock(voice: MessageContent.Voice, textColor: Color, surface: Color? = null) {
    val context = LocalContext.current
    val playback by VoicePlayer.state.collectAsState()
    val mine = playback?.takeIf { it.dispatchIdHex == voice.dispatchIdHex }
    val playing = mine?.playing == true
    val position = mine?.positionMs ?: 0
    val fraction = if (voice.durationMs > 0) (position.toFloat() / voice.durationMs).coerceIn(0f, 1f) else 0f
    val shown = if (mine != null) (voice.durationMs - position).coerceAtLeast(0) else voice.durationMs
    val secs = (shown + 500) / 1000
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
            "%d:%02d".format(Locale.US, secs / 60, secs % 60),
            style = MaterialTheme.typography.labelMedium,
            color = textColor.copy(alpha = 0.8f),
        )
    }
}

/** A player is a control, not prose: one width, whatever the note's length. */
private val VoiceWidth = 232.dp

/** Reserve the sticker's dimensions while downloading to keep chat layout stable. */
@Composable
fun StickerBlock(sticker: MessageContent.Sticker, textColor: Color) {
    val ref = sticker.sticker
    val ratio = (if (ref.width > 0 && ref.height > 0) ref.width.toFloat() / ref.height else 1f)
        .coerceIn(0.5f, 2f)
    val width = if (ratio >= 1f) StickerBox else StickerBox * ratio
    val height = width / ratio
    val bitmap = rememberStickerBitmap(ref)
    Box(
        Modifier
            .size(width, height)
            .clip(RoundedCornerShape(10.dp))
            .then(if (bitmap == null) Modifier.background(textColor.copy(alpha = 0.08f)) else Modifier),
        Alignment.Center,
    ) {
        bitmap?.let { Image(it, null, Modifier.fillMaxSize(), contentScale = ContentScale.Fit) }
    }
}

/** The longest edge a sticker draws at; the other edge follows its aspect. */
private val StickerBox = 160.dp

/** Bars from 0–255 loudness samples; a missing waveform draws as a flat line. */
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
    // Outgoing pre-done = still sending (hash/offer in flight) — you never
    // download your own send, so no glyph and nothing tappable.
    if (outgoing && att.transferState != 2) {
        CircularProgressIndicator(Modifier.size(20.dp), color = textColor, strokeWidth = 2.dp)
        return
    }
    when (att.transferState) {
        // Connecting or downloading — tapping the ring re-drives download():
        // the in-flight guard no-ops a genuinely-live pull, so a tap only
        // force-resumes a stalled one (e.g. one auto-resume hasn't picked up yet).
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

/**
 * A caption line that also reserves the trailing corner meta slot (same trick as
 * the text bubble), so the pinned timestamp never lands on media or the caption's
 * last glyph. Renders as a bare reservation strip when the caption is empty.
 */
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
    Text(
        annotated,
        // A bleeding media block waives the bubble's inset, so the caption puts it
        // back for itself — and the meta lands in the gap the placeholder reserves.
        if (inset) Modifier.padding(start = BubblePadH, end = BubblePadH, top = 4.dp, bottom = BubblePadV)
        else Modifier.padding(top = 4.dp),
        style = style,
        color = textColor,
        inlineContent = inline,
    )
}

/**
 * The file-type mark: a page outline in the text colour, a badge in the bubble
 * accent, and a white label. Three tinted layers so it keeps its two tones in
 * either bubble.
 */
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

fun formatBytes(bytes: Long): String = when {
    bytes < 1024 -> "$bytes B"
    bytes < 1024 * 1024 -> String.format(Locale.US, "%.0f KB", bytes / 1024.0)
    bytes < 1024 * 1024 * 1024 -> String.format(Locale.US, "%.1f MB", bytes / (1024.0 * 1024))
    else -> String.format(Locale.US, "%.1f GB", bytes / (1024.0 * 1024 * 1024))
}
