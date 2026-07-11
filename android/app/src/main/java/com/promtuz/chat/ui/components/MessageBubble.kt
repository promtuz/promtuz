package com.promtuz.chat.ui.components

import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.spring
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.awaitLongPressOrCancellation
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.layout.boundsInRoot
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.foundation.text.InlineTextContent
import androidx.compose.ui.text.Placeholder
import androidx.compose.ui.text.PlaceholderVerticalAlign
import androidx.compose.foundation.text.appendInlineContent
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.em
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.Quote
import com.promtuz.chat.domain.model.ReactionGroup
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.ui.appearance.LocalChatAppearance
import com.promtuz.chat.ui.appearance.LocalChatColors

/**
 * A message bubble as an ordered stack of content blocks (text today; media /
 * reply become sibling blocks with the polymorphic content). Shape/colors/width
 * come from [LocalChatAppearance]. The trailing meta — a sent-time, or a spinner
 * for a not-yet-sent message — is pinned to the bubble's bottom-end corner; a
 * measured inline placeholder keeps that corner glyph-free so text never collides.
 * No per-message ticks: delivery state rides the frontier markers.
 *
 * [onLongPress] (fired with the row's root bounds, for the context-menu lift),
 * [onReactionTap], [onQuoteClick] (fired with the quoted message's dispatch id)
 * and [onDoubleTap] are optional so the bubble stays a pure renderer elsewhere.
 * With [menuState] set, the long-press gesture keeps streaming into the open
 * menu — drag over an item, release to pick it (one continuous pointer stream,
 * same interaction grammar as AppDropMenu).
 */
@Composable
fun MessageBubble(
    modifier: Modifier = Modifier,
    msg: UiMessage,
    mergedTop: Boolean = false,
    mergedBottom: Boolean = false,
    onLongPress: ((Rect) -> Unit)? = null,
    menuState: MessageMenuState? = null,
    onReactionTap: ((String) -> Unit)? = null,
    onQuoteClick: ((String) -> Unit)? = null,
    onDoubleTap: (() -> Unit)? = null,
) {
    val appearance = LocalChatAppearance.current
    val chat = LocalChatColors.current
    val outgoing = msg.outgoing
    val shape = rememberBubbleShape(outgoing, mergedTop, mergedBottom, appearance.bubble)
    val bubbleColor = if (outgoing) chat.outgoingBubble else chat.incomingBubble
    val textColor = if (outgoing) chat.onOutgoingBubble else chat.onIncomingBubble
    val haptic = LocalHapticFeedback.current
    var rowBounds by remember { mutableStateOf(Rect.Zero) }
    var bubbleCoords by remember { mutableStateOf<LayoutCoordinates?>(null) }

    BoxWithConstraints(
        modifier
            .fillMaxWidth()
            .onGloballyPositioned { rowBounds = it.boundsInRoot() }
            .padding(horizontal = 12.dp),
    ) {
        val maxBubble = maxWidth * appearance.layout.maxWidthFraction
        Layout(
            content = {
                msg.quote?.let { q ->
                    QuoteBlock(q, textColor, chat.accent, onQuoteClick?.let { cb -> { cb(q.dispatchIdHex) } })
                }

                BubbleTextWithMeta(msg, textColor, appearance.type.fontScale)

                if (msg.reactions.isNotEmpty()) {
                    Row(
                        Modifier.padding(top = 4.dp),
                        horizontalArrangement = Arrangement.spacedBy(4.dp),
                    ) {
                        msg.reactions.forEach { rg ->
                            ReactionChip(rg, textColor, chat.accent, onReactionTap)
                        }
                    }
                }
            },
            modifier = Modifier
                .align(if (outgoing) Alignment.CenterEnd else Alignment.CenterStart)
                .widthIn(max = maxBubble)
                // edit/delete/reactions change the bubble's size in place — glide from the
                // tail corner, not TopStart, so the anchored edge stays put
                .animateContentSize(
                    spring(stiffness = Spring.StiffnessMediumLow),
                    alignment = if (outgoing) Alignment.BottomEnd else Alignment.BottomStart,
                )
                .clip(shape)
                .background(bubbleColor)
                .onGloballyPositioned { bubbleCoords = it }
                .then(
                    if (onLongPress == null) Modifier
                    else Modifier.pointerInput(menuState) {
                        awaitEachGesture {
                            val down = awaitFirstDown(requireUnconsumed = false)
                            if (menuState?.isOpen == true) return@awaitEachGesture
                            val press = awaitLongPressOrCancellation(down.id) ?: return@awaitEachGesture
                            haptic.performHapticFeedback(HapticFeedbackType.LongPress)
                            onLongPress(rowBounds)
                            if (menuState == null) return@awaitEachGesture

                            // Same finger now drives the open menu: drag hovers, release picks.
                            var dragged = false
                            while (true) {
                                val ev = awaitPointerEvent()
                                val ch = ev.changes.firstOrNull { it.id == press.id } ?: ev.changes.first()
                                val root = bubbleCoords?.takeIf { it.isAttached }?.localToRoot(ch.position)
                                if (!ch.pressed) {
                                    when (val hit = root?.let(menuState::release)) {
                                        is MenuHit.Action -> {
                                            haptic.performHapticFeedback(HapticFeedbackType.Confirm)
                                            hit.action.onClick()
                                        }
                                        is MenuHit.Reaction -> {
                                            haptic.performHapticFeedback(HapticFeedbackType.Confirm)
                                            menuState.onReact?.invoke(hit.emoji)
                                        }
                                        // Drag to nowhere cancels; a plain long-press-release stays open.
                                        null -> if (dragged) menuState.close()
                                    }
                                    break
                                }
                                if (!dragged &&
                                    (ch.position - down.position).getDistance() > viewConfiguration.touchSlop
                                ) dragged = true
                                if (dragged && root != null && menuState.drag(root)) {
                                    haptic.performHapticFeedback(HapticFeedbackType.SegmentTick)
                                }
                                ch.consume()
                            }
                        }
                    }
                )
                .then(
                    if (onDoubleTap == null) Modifier
                    else Modifier.pointerInput(onDoubleTap) {
                        detectTapGestures(onDoubleTap = { onDoubleTap() })
                    }
                )
                .padding(horizontal = 11.dp, vertical = 6.dp),
        ) { measurables, constraints ->
            // Column would leave the quote at its natural width; a quote must span the
            // widest sibling. Each measurable measures once, so the quote goes last with
            // that width as its minimum (a naturally wider quote still wins).
            val hasQuote = msg.quote != null
            val loose = constraints.copy(minWidth = 0, minHeight = 0)
            val text = measurables[if (hasQuote) 1 else 0].measure(loose)
            val reactions = measurables.getOrNull(if (hasQuote) 2 else 1)?.measure(loose)
            val contentWidth = maxOf(text.width, reactions?.width ?: 0)
            val quote = if (hasQuote) measurables[0].measure(loose.copy(minWidth = contentWidth)) else null

            val width = maxOf(contentWidth, quote?.width ?: 0)
            val height = (quote?.height ?: 0) + text.height + (reactions?.height ?: 0)
            layout(width, height) {
                var y = 0
                quote?.let { it.placeRelative(0, 0); y = it.height }
                text.placeRelative(0, y)
                reactions?.placeRelative(0, y + text.height)
            }
        }
    }
}

/** The quoted-message block a reply carries: accent rail + short snippet. */
@Composable
private fun QuoteBlock(quote: Quote, textColor: Color, accent: Color, onClick: (() -> Unit)?) {
    Row(
        Modifier
            .padding(top = 2.dp, bottom = 4.dp)
            .clip(RoundedCornerShape(6.dp))
            .background(textColor.copy(alpha = 0.08f))
            .then(onClick?.let { Modifier.clickable(onClick = it) } ?: Modifier)
            .height(IntrinsicSize.Min),
    ) {
        Box(Modifier.width(3.dp).fillMaxHeight().background(accent))
        Text(
            quote.text ?: "Message unavailable",
            Modifier.padding(horizontal = 8.dp, vertical = 4.dp),
            style = MaterialTheme.typography.bodySmall,
            color = textColor.copy(alpha = if (quote.text != null) 0.8f else 0.5f),
            fontStyle = if (quote.text != null) FontStyle.Normal else FontStyle.Italic,
            maxLines = 2,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

@Composable
private fun ReactionChip(rg: ReactionGroup, textColor: Color, accent: Color, onTap: ((String) -> Unit)?) {
    Row(
        Modifier
            .clip(RoundedCornerShape(10.dp))
            .background(if (rg.mine) accent.copy(alpha = 0.35f) else textColor.copy(alpha = 0.10f))
            .then(onTap?.let { Modifier.clickable { it(rg.emoji) } } ?: Modifier)
            .padding(horizontal = 7.dp, vertical = 3.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(rg.emoji, style = MaterialTheme.typography.labelMedium)
        if (rg.count > 1) Text(
            " ${rg.count}",
            style = MaterialTheme.typography.labelSmall,
            color = textColor.copy(alpha = 0.85f),
        )
    }
}

@Composable
private fun BubbleTextWithMeta(msg: UiMessage, textColor: androidx.compose.ui.graphics.Color, fontScale: Float) {
    val base = MaterialTheme.typography.bodyLarge
    val textStyle = base.copy(fontSize = base.fontSize * fontScale, color = textColor)
    val metaStyle = MaterialTheme.typography.labelSmall
    val metaColor = textColor.copy(alpha = 0.55f)

    val text = if (msg.deleted) "This message was deleted"
    else (msg.content as? MessageContent.Text)?.text.orEmpty()

    val pending = msg.outgoing && msg.status == SendStatus.Pending
    val failed = msg.outgoing && msg.status == SendStatus.Failed
    val timeStr = if (pending || failed) null else clock(msg.timestampMs)
    val edited = msg.edited && !msg.deleted

    // The invisible trailing placeholder reserves exactly the meta's footprint, so the
    // bottom-end corner is glyph-free; the real meta overlays there — corner-true even
    // when an earlier line is longer than the last.
    val density = LocalDensity.current
    val measurer = rememberTextMeasurer()
    val label = buildString {
        if (edited) append("edited ")
        if (timeStr != null) append(timeStr)
    }
    val labelPx = if (label.isNotEmpty()) measurer.measure(label, metaStyle).size.width else 0
    val iconPx = if (pending || failed) with(density) { 14.dp.roundToPx() } else 0
    val gapPx = with(density) { 8.dp.roundToPx() }
    val metaWidth = with(density) { (labelPx + iconPx + gapPx).toSp() }

    val annotated = buildAnnotatedString {
        append(text)
        appendInlineContent("meta")
    }
    val inline = mapOf(
        "meta" to InlineTextContent(
            Placeholder(metaWidth, 1.2.em, PlaceholderVerticalAlign.TextBottom)
        ) {}
    )

    Box {
        Text(
            annotated,
            style = textStyle,
            fontStyle = if (msg.deleted) FontStyle.Italic else FontStyle.Normal,
            color = if (msg.deleted) textColor.copy(alpha = 0.6f) else textColor,
            inlineContent = inline,
        )
        Row(
            Modifier.align(Alignment.BottomEnd),
            verticalAlignment = Alignment.Bottom,
        ) {
            if (edited) Text(
                "edited",
                style = metaStyle,
                color = metaColor,
                modifier = Modifier.padding(end = 4.dp),
            )
            when {
                pending -> CircularProgressIndicator(Modifier.size(11.dp), color = metaColor, strokeWidth = 1.5.dp)
                failed -> Box(Modifier.size(9.dp).clip(CircleShape).background(MaterialTheme.colorScheme.error))
                timeStr != null -> Text(timeStr, style = metaStyle, color = metaColor)
            }
        }
    }
}

private val clockFormat = java.text.SimpleDateFormat("HH:mm", java.util.Locale.getDefault())

private fun clock(ms: Long): String = clockFormat.format(java.util.Date(ms))
