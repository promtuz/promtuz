package com.promtuz.chat.ui.text

import android.content.ClipData
import android.text.SpannableString
import android.text.style.URLSpan
import android.text.util.Linkify
import android.widget.Toast
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.text.InlineTextContent
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.ripple
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Outline
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.input.pointer.PointerIcon
import androidx.compose.ui.input.pointer.PointerInputChange
import androidx.compose.ui.input.pointer.pointerHoverIcon
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalUriHandler
import androidx.compose.ui.platform.LocalViewConfiguration
import androidx.compose.ui.platform.ViewConfiguration
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLayoutResult
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.DpSize
import androidx.compose.ui.unit.IntRect
import androidx.compose.ui.unit.LayoutDirection
import androidx.compose.ui.unit.TextUnit
import androidx.core.text.util.LinkifyCompat
import com.promtuz.chat.R
import com.promtuz.chat.ui.components.MenuAction
import com.promtuz.chat.ui.components.AppDropMenu
import kotlin.math.ceil
import kotlin.math.floor
import kotlinx.coroutines.launch

/** A link owns its entire gesture, including drags that leave its text bounds. */
internal class MessageLinkGestures {
    private var down: PointerInputChange? = null
    fun claim(change: PointerInputChange) { down = change }
    fun owns(change: PointerInputChange): Boolean =
        down?.let { it.id == change.id && it.uptimeMillis == change.uptimeMillis } == true
}

internal val LocalMessageLinkGestures = staticCompositionLocalOf<MessageLinkGestures?> { null }

private data class MessageLink(val url: String, val start: Int, val end: Int)

private class LinkGeometry {
    var shape: Shape by mutableStateOf(object : Shape {
        override fun createOutline(size: Size, layoutDirection: LayoutDirection, density: Density) =
            Outline.Generic(Path())
    })
}

/** Emits exactly one measurable, which the bubble's Layout requires. */
@Composable
fun MessageText(
    text: AnnotatedString,
    modifier: Modifier = Modifier,
    style: TextStyle,
    color: Color,
    inlineContent: Map<String, InlineTextContent> = emptyMap(),
    onTextLayout: ((TextLayoutResult) -> Unit)? = null,
) {
    val links = remember(text.text) {
        val detected = SpannableString(text.text)
        LinkifyCompat.addLinks(detected, Linkify.WEB_URLS or Linkify.EMAIL_ADDRESSES)
        detected.getSpans(0, detected.length, URLSpan::class.java).map {
            MessageLink(it.url, detected.getSpanStart(it), detected.getSpanEnd(it))
        }
    }
    val styled = remember(text, links, color) {
        buildAnnotatedString {
            append(text)
            links.forEach {
                addStyle(SpanStyle(color = color, textDecoration = TextDecoration.Underline), it.start, it.end)
            }
        }
    }
    // Material Text merges unspecified values with LocalTextStyle. Override both,
    // otherwise its ambient 24sp line height would silently come back here.
    val naturalStyle = style.copy(lineHeight = TextUnit.Unspecified)
    CompositionLocalProvider(LocalTextStyle provides naturalStyle) {
        if (links.isEmpty()) {
            EmojiText(text, modifier, style = naturalStyle, color = color,
                inlineContent = inlineContent, onTextLayout = onTextLayout)
            return@CompositionLocalProvider
        }
        val geometries = remember(links) { links.map { LinkGeometry() } }
        val layoutResult = remember(text) { arrayOfNulls<TextLayoutResult>(1) }
        Layout(
            modifier = modifier,
            content = {
                EmojiText(styled, style = naturalStyle, color = color, inlineContent = inlineContent,
                    onTextLayout = { layoutResult[0] = it; onTextLayout?.invoke(it) })
                links.forEachIndexed { index, link ->
                    key(link) {
                        MessageLinkTarget(link, text.text.substring(link.start, link.end), geometries[index], color)
                    }
                }
            },
        ) { measurables, constraints ->
            val content = measurables[0].measure(constraints)
            val result = layoutResult[0]
            val targets = links.mapIndexed { index, link ->
                val path = result?.getPathForRange(link.start, link.end) ?: Path()
                val bounds = path.getBounds().let {
                    IntRect(floor(it.left).toInt(), floor(it.top).toInt(), ceil(it.right).toInt(), ceil(it.bottom).toInt())
                }
                path.translate(-Offset(bounds.left.toFloat(), bounds.top.toFloat()))
                geometries[index].shape = object : Shape {
                    override fun createOutline(size: Size, layoutDirection: LayoutDirection, density: Density) =
                        Outline.Generic(path)
                }
                measurables[index + 1].measure(Constraints.fixed(bounds.width, bounds.height)) to bounds.topLeft
            }
            layout(content.width, content.height) {
                content.place(0, 0)
                targets.forEach { (target, at) -> target.place(at) }
            }
        }
    }
}

@Composable
private fun MessageLinkTarget(link: MessageLink, label: String, geometry: LinkGeometry, color: Color) {
    val gestures = LocalMessageLinkGestures.current
    val context = LocalContext.current
    val uriHandler = LocalUriHandler.current
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    val viewConfiguration = LocalViewConfiguration.current
    val linkConfiguration = remember(viewConfiguration) {
        object : ViewConfiguration by viewConfiguration {
            // Enlarging an inline target to 48dp steals adjacent lines and links.
            override val minimumTouchTargetSize = DpSize.Zero
        }
    }
    val open = {
        runCatching { uriHandler.openUri(link.url) }.onFailure {
            Toast.makeText(context, "Couldn't open this link", Toast.LENGTH_SHORT).show()
        }
        Unit
    }
    CompositionLocalProvider(LocalViewConfiguration provides linkConfiguration) {
        AppDropMenu(
            anchor = { Box(Modifier.fillMaxSize()) },
            groups = listOf(listOf(
                MenuAction("Open link", R.drawable.oi_external_link, onClick = open),
                MenuAction("Copy link", R.drawable.oi_copy) {
                    scope.launch { clipboard.setClipEntry(ClipEntry(ClipData.newPlainText("link", link.url))) }
                },
            )),
            modifier = Modifier
                .graphicsLayer { shape = geometry.shape; clip = true }
                .semantics { contentDescription = label }
                .pointerHoverIcon(PointerIcon.Hand)
                .pointerInput(gestures, link) {
                    awaitEachGesture {
                        val down = awaitFirstDown(requireUnconsumed = false, pass = PointerEventPass.Initial)
                        gestures?.claim(down)
                    }
                },
            onClick = open,
            onClickLabel = "Open link",
            onLongClickLabel = "Link options",
            indication = ripple(color = color),
        )
    }
}
