package com.promtuz.chat.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.unit.dp
import com.promtuz.chat.domain.model.ChatSummary
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.utils.common.parseMessageDate

@Composable
fun HomeChatListItem(
    chat: ChatSummary,
    roundShape: Shape,
    presence: Presence?,
    typing: Boolean,
    onOpen: () -> Unit,
) {
    val type = MaterialTheme.typography
    val colors = MaterialTheme.colorScheme

    Row(
        Modifier
            .fillMaxWidth()
            .clip(roundShape)
            .background(colors.surfaceContainer.copy(0.75f))
            .clickable(onClick = onOpen)
            .padding(vertical = 10.dp, horizontal = 12.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Avatar(chat.name, statusColor = presenceColor(presence))

        Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Row(
                Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.Top,
            ) {
                Text(chat.name, style = type.titleMediumEmphasized, color = colors.onSecondaryContainer)
                if (chat.timestampMs > 0) Text(
                    parseMessageDate(chat.timestampMs),
                    style = type.bodySmallEmphasized,
                    color = colors.onSecondaryContainer.copy(0.5f),
                )
            }
            when {
                typing -> Text(
                    "typing…",
                    style = type.bodySmallEmphasized,
                    color = colors.primary,
                )
                chat.status == 0 -> Text(
                    "Waiting to connect…",
                    style = type.bodySmallEmphasized,
                    color = colors.primary.copy(0.8f),
                )
                chat.status == 2 -> Text(
                    "Couldn't connect",
                    style = type.bodySmallEmphasized,
                    color = colors.error.copy(0.8f),
                )
                else -> chat.lastPreview?.let {
                    Text(it, style = type.bodySmallEmphasized, color = colors.onSecondaryContainer.copy(0.7f))
                }
            }
        }
    }
}

private val OnlineDot = Color(0xFF34C759)
private val IdleDot = Color(0xFFF5A623)

/** Generic status colour for the avatar dot; null hides it (offline/unknown). */
private fun presenceColor(p: Presence?): Color? = when (p) {
    Presence.Online -> OnlineDot
    is Presence.Idle -> IdleDot
    else -> null
}
