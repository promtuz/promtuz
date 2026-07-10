package com.promtuz.chat.ui.components

import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
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
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.em
import com.promtuz.chat.domain.model.MessageContent
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
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun MessageBubble(
    modifier: Modifier = Modifier,
    msg: UiMessage,
    mergedTop: Boolean = false,
    mergedBottom: Boolean = false,
    onLongPress: ((Rect) -> Unit)? = null,
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
                .clip(shape)
                .background(bubbleColor)
                .then(
                    if (onLongPress == null) Modifier
                    else Modifier.combinedClickable(
                        onClick = {},
                        onLongClick = {
                            haptic.performHapticFeedback(HapticFeedbackType.LongPress)
                            onLongPress(rowBounds)
                        },
                    )
                )
                .padding(horizontal = 11.dp, vertical = 6.dp),
        ) {
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
