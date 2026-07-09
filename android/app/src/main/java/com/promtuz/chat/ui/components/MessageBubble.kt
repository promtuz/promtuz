package com.promtuz.chat.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.unit.dp
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.ui.appearance.LocalChatAppearance
import com.promtuz.chat.ui.appearance.incomingBubble
import com.promtuz.chat.ui.appearance.incomingContent
import com.promtuz.chat.ui.appearance.outgoingBubble
import com.promtuz.chat.ui.appearance.outgoingContent

/**
 * A message bubble as an ordered column of content blocks (text today; media /
 * reply / reactions become sibling blocks as the polymorphic content lands).
 * Shape, colors, width and type all come from [LocalChatAppearance] — the bubble
 * is a pure function of (message, group position, appearance).
 */
@Composable
fun MessageBubble(msg: UiMessage, mergedTop: Boolean = false, mergedBottom: Boolean = false) {
    val appearance = LocalChatAppearance.current
    val outgoing = msg.outgoing
    val shape = rememberBubbleShape(outgoing, mergedTop, mergedBottom, appearance.bubble)
    val bubbleColor = if (outgoing) appearance.colors.outgoingBubble() else appearance.colors.incomingBubble()
    val textColor = if (outgoing) appearance.colors.outgoingContent() else appearance.colors.incomingContent()

    BoxWithConstraints(Modifier.fillMaxWidth().padding(horizontal = 12.dp)) {
        val maxBubble = maxWidth * appearance.layout.maxWidthFraction
        Column(
            Modifier
                .align(if (outgoing) Alignment.CenterEnd else Alignment.CenterStart)
                .widthIn(max = maxBubble)
                .clip(shape)
                .background(bubbleColor)
                .padding(horizontal = 12.dp, vertical = 7.dp),
        ) {
            Text(
                text = if (msg.deleted) "This message was deleted"
                else (msg.content as? MessageContent.Text)?.text.orEmpty(),
                style = MaterialTheme.typography.bodyLarge.copy(
                    fontSize = MaterialTheme.typography.bodyLarge.fontSize * appearance.type.fontScale,
                ),
                color = if (msg.deleted) textColor.copy(alpha = 0.6f) else textColor,
                fontStyle = if (msg.deleted) FontStyle.Italic else FontStyle.Normal,
            )

            if (msg.reactions.isNotEmpty()) {
                Row(
                    Modifier.padding(top = 4.dp),
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    msg.reactions.forEach {
                        Text("${it.emoji} ${it.count}", style = MaterialTheme.typography.labelSmall, color = textColor)
                    }
                }
            }

            Row(
                Modifier.align(Alignment.End).padding(top = 2.dp),
                horizontalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                if (msg.edited && !msg.deleted) Text(
                    "edited",
                    style = MaterialTheme.typography.labelSmall,
                    color = textColor.copy(alpha = 0.6f),
                )
                if (outgoing) Text(
                    tick(msg.status),
                    style = MaterialTheme.typography.labelSmall,
                    color = textColor.copy(alpha = 0.7f),
                )
            }
        }
    }
}

private fun tick(status: SendStatus): String = when (status) {
    SendStatus.Pending -> "🕓"
    SendStatus.Sent -> "✓"
    SendStatus.Delivered, SendStatus.Read -> "✓✓"
    SendStatus.Failed -> "!"
}
