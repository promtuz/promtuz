package com.promtuz.chat.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.RoundedCornerShape
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

/**
 * Bare message bubble — the reactive-foundation placeholder. The polished,
 * customizable, animated bubble (content-block variants, BubbleShape, reactions
 * bar, morph) is the next sub-project; this just renders text + state correctly.
 */
@Composable
fun MessageBubble(msg: UiMessage) {
    val colors = MaterialTheme.colorScheme
    val outgoing = msg.outgoing
    Row(
        Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 2.dp),
        horizontalArrangement = if (outgoing) Arrangement.End else Arrangement.Start,
    ) {
        Column(
            Modifier
                .widthIn(max = 300.dp)
                .clip(RoundedCornerShape(16.dp))
                .background(if (outgoing) colors.primaryContainer else colors.surfaceContainerHigh)
                .padding(horizontal = 12.dp, vertical = 8.dp),
        ) {
            Text(
                text = if (msg.deleted) "This message was deleted"
                else (msg.content as? MessageContent.Text)?.text.orEmpty(),
                style = MaterialTheme.typography.bodyLarge,
                color = if (msg.deleted) colors.onSurfaceVariant else colors.onSurface,
                fontStyle = if (msg.deleted) FontStyle.Italic else FontStyle.Normal,
            )

            if (msg.reactions.isNotEmpty()) {
                Row(
                    Modifier.padding(top = 4.dp),
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    msg.reactions.forEach {
                        Text("${it.emoji} ${it.count}", style = MaterialTheme.typography.labelSmall)
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
                    color = colors.onSurfaceVariant,
                )
                if (outgoing) Text(
                    tick(msg.status),
                    style = MaterialTheme.typography.labelSmall,
                    color = colors.onSurfaceVariant,
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
