package com.promtuz.chat.ui.components

import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.spring
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.awaitLongPressOrCancellation
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
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
 * A message bubble as an ordered column of content blocks (text today; media /
 * reply become sibling blocks with the polymorphic content). Shape/colors/width
 * come from [LocalChatAppearance]. The trailing meta — a sent-time, or a spinner
 * for a not-yet-sent message — tucks into the last text line's trailing space and
 * only wraps below when there's genuinely no room (via a measured inline
 * placeholder). No per-message ticks: delivery state rides the frontier markers.
 *
 * [onLongPress] (fired with the row's root bounds, for the context-menu lift) and
 * [onReactionTap] are optional so the bubble stays a pure renderer elsewhere.
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
        Column(
            Modifier
                .align(if (outgoing) Alignment.CenterEnd else Alignment.CenterStart)
                .widthIn(max = maxBubble)
                // edit/delete/reactions change the bubble's size in place — glide, don't snap
                .animateContentSize(spring(stiffness = Spring.StiffnessMediumLow))
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
                .padding(horizontal = 11.dp, vertical = 6.dp),
        ) {
            msg.quote?.let { QuoteBlock(it, textColor, chat.accent) }

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
        }
    }
}

/** The quoted-message block a reply carries: accent rail + short snippet. */
@Composable
private fun QuoteBlock(quote: Quote, textColor: Color, accent: Color) {
    Row(
        Modifier
            .padding(top = 2.dp, bottom = 4.dp)
            .clip(RoundedCornerShape(6.dp))
            .background(textColor.copy(alpha = 0.08f))
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

    // Reserve exactly the meta's width at the end of the text so it tucks into the last line's
    // trailing gap, wrapping to its own (short) line only when the line is genuinely full.
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
        ) {
            Row(
                Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.End,
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
    )

    Text(
        annotated,
        style = textStyle,
        fontStyle = if (msg.deleted) FontStyle.Italic else FontStyle.Normal,
        color = if (msg.deleted) textColor.copy(alpha = 0.6f) else textColor,
        inlineContent = inline,
    )
}

private val clockFormat = java.text.SimpleDateFormat("HH:mm", java.util.Locale.getDefault())

private fun clock(ms: Long): String = clockFormat.format(java.util.Date(ms))
