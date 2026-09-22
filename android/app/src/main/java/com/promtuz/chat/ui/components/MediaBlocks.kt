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
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.MessageContent
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
) {
    val ratio = (if (image.width > 0 && image.height > 0) image.width.toFloat() / image.height else 1f)
        .coerceIn(0.6f, 1.9f)
    Column {
        // No clip of its own: the picture runs to the bubble's edge and the
        // bubble's shape does the rounding, so there's one outline, not two.
        Box(
            Modifier
                .fillMaxWidth()
                .aspectRatio(ratio)
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
 * An album: its members in a square grid, one visual unit for what are still
 * separate messages underneath.
 *
 * Columns come from the count rather than a fixed grid — two photos side by side
 * read as a pair, where forcing them into a 3-wide row leaves a hole. Cells are
 * cropped square so a mixed-orientation pick still tiles evenly.
 */
@Composable
fun AlbumBlock(
    album: MessageContent.Album, textColor: Color, fontScale: Float, metaLabel: String,
    outgoing: Boolean = false,
) {
    val cols = when {
        album.items.size <= 2 -> album.items.size.coerceAtLeast(1)
        album.items.size == 4 -> 2
        else -> 3
    }
    Column {
        Column(verticalArrangement = Arrangement.spacedBy(AlbumGap)) {
            album.items.chunked(cols).forEach { row ->
                Row(
                    Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(AlbumGap),
                ) {
                    row.forEach { item ->
                        Box(
                            Modifier
                                .weight(1f)
                                .aspectRatio(1f)
                                .background(textColor.copy(alpha = 0.10f)),
                        ) {
                            albumBitmap(item.content)?.let {
                                Image(it, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop)
                            }
                        }
                    }
                    // A short last row keeps its cells the same size as the rest
                    // instead of stretching to fill the width.
                    repeat(cols - row.size) { Spacer(Modifier.weight(1f)) }
                }
            }
        }
        if (album.caption.isNotEmpty()) {
            Caption(album.caption, textColor, fontScale, metaLabel, outgoing, inset = true)
        }
    }
}

/** Whatever an album member can draw: an inline image's bitmap, a file's thumb. */
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
                    ?: Text(glyphFor(att.mime), style = MaterialTheme.typography.titleMedium)
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

private fun glyphFor(mime: String): String = when {
    mime.startsWith("image/") -> "🖼️" // framed picture
    mime.startsWith("video/") -> "🎬" // clapper
    mime.startsWith("audio/") -> "🎵" // note
    mime == "application/pdf" -> "📄" // page
    else -> "📎" // paperclip
}

fun formatBytes(bytes: Long): String = when {
    bytes < 1024 -> "$bytes B"
    bytes < 1024 * 1024 -> String.format(Locale.US, "%.0f KB", bytes / 1024.0)
    bytes < 1024 * 1024 * 1024 -> String.format(Locale.US, "%.1f MB", bytes / (1024.0 * 1024))
    else -> String.format(Locale.US, "%.1f GB", bytes / (1024.0 * 1024 * 1024))
}
