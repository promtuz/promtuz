package com.promtuz.chat.ui.components

import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.Animatable
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.awaitLongPressOrCancellation
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
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
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.em
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.Quote
import com.promtuz.chat.domain.model.ReactionGroup
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.ui.appearance.LocalChatAppearance
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.stage.ChatMotion

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
    onDownload: ((String) -> Unit)? = null,
    onOpen: ((String) -> Unit)? = null,
    peerName: String = "",
) {
    val appearance = LocalChatAppearance.current
    val chat = LocalChatColors.current
    val outgoing = msg.outgoing
    val shape = rememberBubbleShape(outgoing, mergedTop, mergedBottom, appearance.bubble)
    val bubbleColor = if (outgoing) chat.outgoingBubble else chat.incomingBubble
    val textColor = if (outgoing) chat.onOutgoingBubble else chat.onIncomingBubble
    val haptic = LocalHapticFeedback.current
    // Plain refs, not snapshot state: positions change every frame during placement
    // animations and are only ever read inside gesture handlers.
    val coords = remember { CoordsHolder() }

    // Plain Box, not BoxWithConstraints — that's a nested SubcomposeLayout per
    // bubble, real weight on every bubble birth. The width cap is applied inside
    // the bubble Layout's own measure from its incoming constraints.
    val widthFraction = appearance.layout.maxWidthFraction
    Box(
        modifier
            .fillMaxWidth()
            .onGloballyPositioned { coords.row = it }
            .padding(horizontal = 12.dp),
        contentAlignment = if (outgoing) Alignment.CenterEnd else Alignment.CenterStart,
    ) {
        Layout(
            content = {
                msg.quote?.let { q ->
                    QuoteBlock(q, textColor, chat.accent, onQuoteClick?.let { cb -> { cb(q.dispatchIdHex) } })
                }

                // One content child in a fixed slot: the bubble Layout hardcodes child
                // indices, so a deleted-or-text bubble and a media bubble must emit exactly
                // one measurable here. The meta corner is reserved inside each variant.
                val content = msg.content
                when {
                    msg.deleted || content is MessageContent.Text ->
                        BubbleText(msg, textColor, appearance.type.fontScale)
                    content is MessageContent.Image ->
                        ImageBlock(content, textColor, appearance.type.fontScale, BubbleTextLayouts.metaLabelOf(msg))
                    content is MessageContent.Attachment ->
                        AttachmentBlock(
                            content, textColor, appearance.type.fontScale,
                            BubbleTextLayouts.metaLabelOf(msg), peerName, onDownload, onOpen,
                        )
                }

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

                MetaRow(msg, textColor)
            },
            modifier = Modifier
                // edit/delete/reactions change the bubble's size in place — glide from the
                // tail corner on the shared clock so neighbors (stage) track frame-locked
                .animateContentSize(
                    ChatMotion.spec(),
                    alignment = if (outgoing) Alignment.BottomEnd else Alignment.BottomStart,
                )
                .clip(shape)
                .background(bubbleColor)
                .onGloballyPositioned { coords.bubble = it }
                .then(
                    if (onLongPress == null) Modifier
                    else Modifier.pointerInput(menuState) {
                        awaitEachGesture {
                            val down = awaitFirstDown(requireUnconsumed = false)
                            if (menuState?.isOpen == true) return@awaitEachGesture
                            val press = awaitLongPressOrCancellation(down.id) ?: return@awaitEachGesture
                            haptic.performHapticFeedback(HapticFeedbackType.LongPress)
                            onLongPress(coords.row?.takeIf { it.isAttached }?.boundsInRoot() ?: Rect.Zero)
                            if (menuState == null) return@awaitEachGesture

                            // Same finger now drives the open menu: drag hovers, release picks.
                            var dragged = false
                            while (true) {
                                val ev = awaitPointerEvent()
                                val ch = ev.changes.firstOrNull { it.id == press.id } ?: ev.changes.first()
                                val root = coords.bubble?.takeIf { it.isAttached }?.localToRoot(ch.position)
                                if (!ch.pressed) {
                                    // Commits require an actual drag: a stationary hold-and-lift
                                    // only leaves the menu open, even if an item spawned under
                                    // the finger. Drag to nowhere cancels.
                                    if (dragged) when (val hit = root?.let(menuState::release)) {
                                        is MenuHit.Action -> {
                                            haptic.performHapticFeedback(HapticFeedbackType.Confirm)
                                            hit.action.onClick()
                                        }
                                        is MenuHit.Reaction -> {
                                            haptic.performHapticFeedback(HapticFeedbackType.Confirm)
                                            menuState.onReact?.invoke(hit.emoji)
                                        }
                                        null -> menuState.close()
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
            // Children: [quote?] text [reactions?] meta. The quote must span the widest
            // sibling (measured last with that width as its minimum — measurables measure
            // once); the meta is pinned to the bubble's absolute bottom-end corner — the
            // text reserves that corner via its invisible trailing placeholder, and a
            // reactions row shares its line with the meta instead.
            val hasQuote = msg.quote != null
            val hasReactions = msg.reactions.isNotEmpty()
            val cap = (constraints.maxWidth * widthFraction).toInt()
            val loose = Constraints(maxWidth = cap)
            var idx = if (hasQuote) 1 else 0
            val text = measurables[idx].measure(loose)
            val reactions = if (hasReactions) measurables[++idx].measure(loose) else null
            val meta = measurables[idx + 1].measure(loose)

            val metaGap = 8.dp.roundToPx()
            val contentWidth = maxOf(
                text.width,
                reactions?.let { it.width + metaGap + meta.width } ?: 0,
            )
            val quote = if (hasQuote) measurables[0].measure(loose.copy(minWidth = contentWidth)) else null

            val width = maxOf(contentWidth, quote?.width ?: 0)
            val height = (quote?.height ?: 0) + text.height + (reactions?.height ?: 0)
            layout(width, height) {
                var y = 0
                quote?.let { it.placeRelative(0, 0); y = it.height }
                text.placeRelative(0, y)
                reactions?.placeRelative(0, y + text.height)
                meta.placeRelative(width - meta.width, height - meta.height)
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

/**
 * The message text with an invisible trailing placeholder reserving the meta's
 * final footprint — the timestamp is known at send time, so the flip never
 * re-wraps a line. The meta itself is a sibling pinned to the absolute corner.
 */
@Composable
private fun BubbleText(msg: UiMessage, textColor: Color, fontScale: Float) {
    val color = if (msg.deleted) textColor.copy(alpha = 0.6f) else textColor
    val text = BubbleTextLayouts.contentOf(msg)

    val base = MaterialTheme.typography.bodyLarge
    val metaStyle = MaterialTheme.typography.labelSmall
    val density = LocalDensity.current
    val measurer = rememberTextMeasurer()
    val label = BubbleTextLayouts.metaLabelOf(msg)
    val labelPx = remember(label, metaStyle) { measurer.measure(label, metaStyle).size.width }
    val metaWidth = with(density) { (labelPx + 8.dp.roundToPx()).toSp() }

    val annotated = buildAnnotatedString {
        append(text)
        appendInlineContent("meta")
    }
    val inline = mapOf(
        "meta" to InlineTextContent(
            Placeholder(metaWidth, 1.2.em, PlaceholderVerticalAlign.TextBottom)
        ) {}
    )
    Text(
        annotated,
        Modifier.fadeOnChange(text),
        style = base.copy(fontSize = base.fontSize * fontScale, color = color),
        fontStyle = if (msg.deleted) FontStyle.Italic else FontStyle.Normal,
        color = color,
        inlineContent = inline,
    )
}

/** Fades the node in when [value] changes after first composition. Near-free at rest. */
@Composable
private fun Modifier.fadeOnChange(value: Any?): Modifier {
    val anim = remember { Animatable(1f) }
    var last by remember { mutableStateOf(value) }
    LaunchedEffect(value) {
        if (last != value) {
            last = value
            anim.snapTo(0f)
            anim.animateTo(1f, ChatMotion.spec())
        }
    }
    return graphicsLayer { alpha = anim.value }
}

/** Pending spinner / failed dot / sent time, crossfading inside the corner slot. */
@Composable
private fun MetaRow(msg: UiMessage, textColor: Color) {
    val metaStyle = MaterialTheme.typography.labelSmall
    val metaColor = textColor.copy(alpha = 0.55f)
    val edited = msg.edited && !msg.deleted
    val state = when {
        msg.outgoing && msg.status == SendStatus.Pending -> MetaState.Pending
        msg.outgoing && msg.status == SendStatus.Failed -> MetaState.Failed
        else -> MetaState.Sent
    }

    Row(verticalAlignment = Alignment.CenterVertically) {
        if (edited) Text(
            "edited",
            style = metaStyle,
            color = metaColor,
            modifier = Modifier.padding(end = 4.dp),
        )
        Box(Modifier.fadeOnChange(state), contentAlignment = Alignment.CenterEnd) {
            when (state) {
                MetaState.Pending ->
                    CircularProgressIndicator(Modifier.size(11.dp), color = metaColor, strokeWidth = 1.5.dp)
                MetaState.Failed ->
                    Box(Modifier.size(9.dp).clip(CircleShape).background(MaterialTheme.colorScheme.error))
                MetaState.Sent ->
                    Text(BubbleTextLayouts.clock(msg.timestampMs), style = metaStyle, color = metaColor)
            }
        }
    }
}

private enum class MetaState { Pending, Failed, Sent }

private class CoordsHolder {
    var row: LayoutCoordinates? = null
    var bubble: LayoutCoordinates? = null
}

