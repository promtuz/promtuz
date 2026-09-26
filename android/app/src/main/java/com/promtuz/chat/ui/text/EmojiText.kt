package com.promtuz.chat.ui.text

import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.text.InlineTextContent
import androidx.compose.foundation.text.appendInlineContent
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.Placeholder
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.PlaceholderVerticalAlign
import androidx.compose.ui.text.TextLayoutResult
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.unit.em

/** How much of the line an emoji glyph takes, relative to the font size; a touch over 1 matches how system emoji sit. */
internal const val EmojiSizeEm = 1.2f
private val EmojiEm = EmojiSizeEm.em

/**
 * [Text] that draws emoji from the bundled pack, so a message or a reaction
 * looks the same on every device. Plain text, and text the pack cannot draw,
 * goes through the ordinary path untouched. The original string is what gets
 * laid out, copied and read aloud: each glyph is inline content whose
 * alternate text is the cluster it replaces.
 */
@Composable
fun EmojiText(
    text: String,
    modifier: Modifier = Modifier,
    style: TextStyle = LocalTextStyle.current,
    color: Color = Color.Unspecified,
    fontSize: TextUnit = TextUnit.Unspecified,
    fontStyle: FontStyle? = null,
    maxLines: Int = Int.MAX_VALUE,
    overflow: TextOverflow = TextOverflow.Clip,
    softWrap: Boolean = true,
    onTextLayout: ((TextLayoutResult) -> Unit)? = null,
) = EmojiText(
    AnnotatedString(text), modifier, style, color, fontSize, fontStyle,
    maxLines, overflow, softWrap, onTextLayout,
)

/** Preserve caller annotations and inline content, including a caption's timestamp reservation. */
@Composable
fun EmojiText(
    text: AnnotatedString,
    modifier: Modifier = Modifier,
    style: TextStyle = LocalTextStyle.current,
    color: Color = Color.Unspecified,
    fontSize: TextUnit = TextUnit.Unspecified,
    fontStyle: FontStyle? = null,
    maxLines: Int = Int.MAX_VALUE,
    overflow: TextOverflow = TextOverflow.Clip,
    softWrap: Boolean = true,
    onTextLayout: ((TextLayoutResult) -> Unit)? = null,
    inlineContent: Map<String, InlineTextContent> = emptyMap(),
) {
    val context = LocalContext.current
    EmojiPack.ensureLoaded(context)
    val index by EmojiPack.index.collectAsState()
    val runs = remember(text, index) {
        index?.let { EmojiSequences.split(text.text, it.keys, it.aliases) } ?: emptyList()
    }
    if (EmojiSequences.isPlain(runs)) {
        Text(
            text, modifier, color = color, fontSize = fontSize, fontStyle = fontStyle,
            maxLines = maxLines, overflow = overflow, softWrap = softWrap,
            onTextLayout = onTextLayout ?: {}, style = style, inlineContent = inlineContent,
        )
        return
    }
    val annotated = remember(text, runs) {
        buildAnnotatedString {
            append(text)
            var offset = 0
            for (run in runs) {
                val length = when (run) {
                    is EmojiRun.Text -> run.text.length
                    is EmojiRun.Emoji -> run.cluster.length
                }
                if (run is EmojiRun.Emoji) {
                    // Use appendInlineContent's public annotation contract, retaining the
                    // original spans, offsets and Unicode rather than rebuilding the text.
                    val marker = buildAnnotatedString { appendInlineContent("emoji:${run.key}", run.cluster) }
                    for (annotation in marker.getStringAnnotations(0, marker.length)) {
                        addStringAnnotation(annotation.tag, annotation.item, offset, offset + length)
                    }
                }
                offset += length
            }
        }
    }
    val glyphStyle = style.merge(TextStyle(color = color, fontSize = fontSize, fontStyle = fontStyle))
    val inline = remember(runs, glyphStyle, inlineContent) {
        inlineContent + runs.filterIsInstance<EmojiRun.Emoji>().associate { run ->
            "emoji:${run.key}" to InlineTextContent(
                Placeholder(EmojiEm, EmojiEm, PlaceholderVerticalAlign.TextCenter),
            ) { cluster -> EmojiGlyph(run.key, cluster, glyphStyle) }
        }
    }
    Text(
        annotated, modifier, color = color, fontSize = fontSize, fontStyle = fontStyle,
        maxLines = maxLines, overflow = overflow, softWrap = softWrap,
        inlineContent = inline, onTextLayout = onTextLayout ?: {}, style = style,
    )
}

/** One glyph from the pack, or the system's rendering of [cluster] until it has decoded. */
@Composable
private fun EmojiGlyph(assetKey: String, cluster: String, style: TextStyle) {
    val context = LocalContext.current
    // produceState's initial value is remembered, even when its producer key changes.
    // Give each asset its own state so an edit cannot display the previous emoji.
    key(assetKey) {
        val glyph by produceState(EmojiPack.peek(assetKey), assetKey) {
            value = EmojiPack.glyph(context, assetKey)
        }
        val bitmap = glyph
        if (bitmap != null) Image(bitmap, null, Modifier.fillMaxSize(), contentScale = ContentScale.Fit)
        else Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
            Text(cluster, style = style, maxLines = 1, softWrap = false)
        }
    }
}
