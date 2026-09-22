package com.promtuz.chat.ui.text

import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.text.InlineTextContent
import androidx.compose.foundation.text.appendInlineContent
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.Placeholder
import androidx.compose.ui.text.PlaceholderVerticalAlign
import androidx.compose.ui.text.TextLayoutResult
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.unit.em

/** How much of the line an emoji glyph takes, relative to the font size; a touch over 1 matches how system emoji sit. */
private val EmojiEm = 1.2.em

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
) {
    val context = LocalContext.current
    EmojiPack.ensureLoaded(context)
    val index by EmojiPack.index.collectAsState()
    val runs = remember(text, index) {
        index?.let { EmojiSequences.split(text, it.keys, it.aliases) } ?: emptyList()
    }
    if (EmojiSequences.isPlain(runs)) {
        Text(
            text, modifier, color = color, fontSize = fontSize, fontStyle = fontStyle,
            maxLines = maxLines, overflow = overflow, softWrap = softWrap,
            onTextLayout = onTextLayout, style = style,
        )
        return
    }
    val annotated = remember(runs) {
        buildAnnotatedString {
            for (run in runs) when (run) {
                is EmojiRun.Text -> append(run.text)
                is EmojiRun.Emoji -> appendInlineContent(run.key, run.cluster)
            }
        }
    }
    val inline = remember(runs) {
        runs.filterIsInstance<EmojiRun.Emoji>().associate { run ->
            run.key to InlineTextContent(
                Placeholder(EmojiEm, EmojiEm, PlaceholderVerticalAlign.TextCenter),
            ) { EmojiGlyph(run.key, run.cluster) }
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
private fun EmojiGlyph(key: String, cluster: String) {
    val context = LocalContext.current
    val glyph by produceState(EmojiPack.peek(key), key) { value = EmojiPack.glyph(context, key) }
    val bitmap = glyph
    if (bitmap != null) Image(bitmap, null, Modifier.fillMaxSize(), contentScale = ContentScale.Fit)
    else Text(cluster, maxLines = 1, softWrap = false)
}
