package com.promtuz.chat.ui.components

import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.InlineTextContent
import androidx.compose.foundation.text.appendInlineContent
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.Placeholder
import androidx.compose.ui.text.PlaceholderVerticalAlign
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.em
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.MessageContent
import java.util.Locale

private val MediaRadius = RoundedCornerShape(14.dp)

/**
 * Inline image. The box reserves the image's aspect-ratio footprint from
 * [width]/[height] so the windowed stage keeps a stable row height whether or
 * not [MessageContent.Image.bitmap] decoded (null on API levels without AVIF) —
 * a null bitmap shows a muted stand-in of the same size, never a collapse.
 */
@Composable
fun ImageBlock(image: MessageContent.Image, textColor: Color, fontScale: Float, metaLabel: String) {
    val ratio = (if (image.width > 0 && image.height > 0) image.width.toFloat() / image.height else 1f)
        .coerceIn(0.6f, 1.9f)
    Column {
        Box(
            Modifier
                .fillMaxWidth()
                .aspectRatio(ratio)
                .clip(MediaRadius)
                .background(textColor.copy(alpha = 0.10f)),
        ) {
            image.bitmap?.let {
                Image(it, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop)
            }
        }
        Caption(image.caption, textColor, fontScale, metaLabel)
    }
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
    onDownload: ((String) -> Unit)?,
    onOpen: ((String) -> Unit)?,
) {
    val active = att.transferState == 1
    val subtitle = if (active && att.transferTotal > 0)
        "${formatBytes(att.size)} · ${att.transferHave * 100 / att.transferTotal}%"
    else formatBytes(att.size)
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
            TransferAffordance(att, textColor, onDownload, onOpen)
        }
        Caption(att.caption, textColor, fontScale, metaLabel)
    }
}

@Composable
private fun TransferAffordance(
    att: MessageContent.Attachment,
    textColor: Color,
    onDownload: ((String) -> Unit)?,
    onOpen: ((String) -> Unit)?,
) {
    when (att.transferState) {
        1 -> if (att.transferTotal > 0)
            CircularProgressIndicator(
                progress = { att.transferHave.toFloat() / att.transferTotal },
                modifier = Modifier.size(26.dp),
                color = textColor,
                strokeWidth = 2.dp,
            )
        else CircularProgressIndicator(Modifier.size(26.dp), color = textColor, strokeWidth = 2.dp)
        2 -> IconButton({ att.localPath?.let { onOpen?.invoke(it) } }) {
            DrawableIcon(R.drawable.i_check, Modifier.size(20.dp), tint = textColor)
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
private fun Caption(text: String, textColor: Color, fontScale: Float, metaLabel: String) {
    val style = if (text.isEmpty()) MaterialTheme.typography.labelSmall
    else MaterialTheme.typography.bodyLarge.let { it.copy(fontSize = it.fontSize * fontScale) }
    val density = LocalDensity.current
    val measurer = rememberTextMeasurer()
    val metaStyle = MaterialTheme.typography.labelSmall
    val labelPx = remember(metaLabel, metaStyle) { measurer.measure(metaLabel, metaStyle).size.width }
    val metaWidth = with(density) { (labelPx + 8.dp.roundToPx()).toSp() }

    val annotated = buildAnnotatedString {
        append(text)
        appendInlineContent("meta")
    }
    val inline = mapOf(
        "meta" to InlineTextContent(Placeholder(metaWidth, 1.2.em, PlaceholderVerticalAlign.TextBottom)) {}
    )
    Text(
        annotated,
        Modifier.padding(top = 4.dp),
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
