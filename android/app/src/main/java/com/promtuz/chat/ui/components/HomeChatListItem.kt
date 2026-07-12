package com.promtuz.chat.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.indication
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.PressInteraction
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.ColorScheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.ripple
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.promtuz.chat.domain.model.ChatSummary
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.utils.common.parseMessageDate
import kotlinx.coroutines.withTimeoutOrNull

private enum class DownResult { TAP, SCROLL, GONE }

@Composable
fun HomeChatListItem(
    chat: ChatSummary,
    presence: Presence?,
    typing: Boolean,
    pinned: Boolean,
    muted: Boolean,
    menuState: HomeMenuState,
    onOpen: () -> Unit,
    onPin: () -> Unit,
    onMute: () -> Unit,
    onMarkRead: () -> Unit,
    onDelete: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val type = MaterialTheme.typography
    val colors = MaterialTheme.colorScheme
    val haptic = LocalHapticFeedback.current
    val unread = chat.unreadCount > 0

    var confirmDelete by remember { mutableStateOf(false) }
    val interaction = remember { MutableInteractionSource() }
    val rowCoord = remember { object { var c: LayoutCoordinates? = null } }

    val groups = listOf(
        buildList {
            add(MenuAction(if (pinned) "Unpin" else "Pin") { onPin() })
            add(MenuAction(if (muted) "Unmute" else "Mute") { onMute() })
            if (unread) add(MenuAction("Mark read") { onMarkRead() })
        },
        listOf(MenuAction("Delete chat", destructive = true) { confirmDelete = true }),
    )

    Box(modifier) {
        Row(
            Modifier
                .fillMaxWidth()
                .onGloballyPositioned { rowCoord.c = it }
                .indication(interaction, ripple())
                // One gesture arbitrates tap / scroll / long-press: a quick lift opens
                // the chat, a pre-timeout drag lets the list scroll, and a stationary
                // hold opens the menu — then the SAME finger drags to an item and
                // releases to pick it (the iconic hold-and-swipe).
                .pointerInput(chat.peerHex, pinned, muted, unread) {
                    awaitEachGesture {
                        val down = awaitFirstDown(requireUnconsumed = false)
                        if (menuState.isOpen) return@awaitEachGesture
                        val press = PressInteraction.Press(down.position)
                        interaction.tryEmit(press)
                        try {
                            val res = withTimeoutOrNull(viewConfiguration.longPressTimeoutMillis) {
                                while (true) {
                                    val ev = awaitPointerEvent()
                                    val ch = ev.changes.firstOrNull { it.id == down.id }
                                        ?: return@withTimeoutOrNull DownResult.GONE
                                    if (!ch.pressed) return@withTimeoutOrNull DownResult.TAP
                                    if ((ch.position - down.position).getDistance() > viewConfiguration.touchSlop)
                                        return@withTimeoutOrNull DownResult.SCROLL
                                }
                                @Suppress("UNREACHABLE_CODE") DownResult.SCROLL
                            }
                            when (res) {
                                DownResult.TAP -> onOpen()
                                null -> {
                                    haptic.performHapticFeedback(HapticFeedbackType.LongPress)
                                    val at = rowCoord.c?.takeIf { it.isAttached }?.localToRoot(down.position)
                                        ?: Offset.Zero
                                    menuState.open(HomeMenuAnchor(at, groups))
                                    var dragged = false
                                    while (true) {
                                        val ev = awaitPointerEvent()
                                        val ch = ev.changes.firstOrNull { it.id == down.id } ?: ev.changes.first()
                                        val root = rowCoord.c?.takeIf { it.isAttached }?.localToRoot(ch.position)
                                        if (!ch.pressed) {
                                            if (dragged) {
                                                val a = root?.let(menuState::release)
                                                if (a != null) {
                                                    haptic.performHapticFeedback(HapticFeedbackType.Confirm)
                                                    a.onClick()
                                                }
                                                menuState.close()
                                            }
                                            break
                                        }
                                        if (!dragged &&
                                            (ch.position - down.position).getDistance() > viewConfiguration.touchSlop
                                        ) dragged = true
                                        if (dragged && root != null && menuState.drag(root))
                                            haptic.performHapticFeedback(HapticFeedbackType.SegmentTick)
                                        ch.consume()
                                    }
                                }
                                else -> {} // SCROLL / GONE — leave it for the list to scroll
                            }
                        } finally {
                            interaction.tryEmit(PressInteraction.Release(press))
                        }
                    }
                }
                .padding(horizontal = 16.dp, vertical = 9.dp),
            horizontalArrangement = Arrangement.spacedBy(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Avatar(chat.name, statusColor = presenceColor(presence))

            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(3.dp)) {
                Row(
                    Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        chat.name,
                        Modifier.weight(1f),
                        style = type.titleMediumEmphasized,
                        fontWeight = if (unread) FontWeight.Bold else null,
                        color = colors.onSurface,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    if (chat.timestampMs > 0) Row(
                        horizontalArrangement = Arrangement.spacedBy(3.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        if (chat.lastOutgoing && !typing) DeliveryTick(chat.lastStatus)
                        Text(
                            parseMessageDate(chat.timestampMs),
                            style = type.bodySmallEmphasized,
                            color = if (unread && !muted) colors.primary else colors.onSurfaceVariant.copy(0.7f),
                        )
                    }
                }

                Row(
                    Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    val (line, lineColor) = statusLine(chat, typing, colors)
                    Text(
                        line,
                        Modifier.weight(1f),
                        style = type.bodySmallEmphasized,
                        color = lineColor,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    if (unread) UnreadBadge(chat.unreadCount, muted, colors)
                }
            }
        }

        if (confirmDelete) DeleteChatDialog(
            name = chat.name,
            onConfirm = { confirmDelete = false; onDelete() },
            onDismiss = { confirmDelete = false },
        )
    }
}

/** The preview/status line: live typing beats pairing state beats last message. */
private fun statusLine(chat: ChatSummary, typing: Boolean, colors: ColorScheme): Pair<String, Color> = when {
    typing -> "typing…" to colors.primary
    chat.status == 0 -> "Waiting to connect…" to colors.primary.copy(0.8f)
    chat.status == 2 -> declineText(chat.rejectReason) to colors.error.copy(0.85f)
    chat.lastDeleted -> "deleted message" to colors.onSurfaceVariant.copy(0.6f)
    chat.lastPreview.isNullOrEmpty() -> "No messages yet" to colors.onSurfaceVariant.copy(0.6f)
    else -> {
        val text = if (chat.lastOutgoing) "You: ${chat.lastPreview}" else chat.lastPreview
        val col = if (chat.unreadCount > 0) colors.onSurface.copy(0.9f) else colors.onSurfaceVariant.copy(0.7f)
        text to col
    }
}

/** "Couldn't connect" plus the decline reason (DECLINE_* code), when known. */
private fun declineText(reason: Int?): String = when (reason) {
    0 -> "Couldn't connect — secure group failed"
    1 -> "Couldn't connect — their invite was already used"
    2 -> "Couldn't connect — they declined"
    else -> "Couldn't connect"
}

/** Delivery tick for our last message; nothing while still pending. */
@Composable
private fun DeliveryTick(status: Int) {
    val colors = MaterialTheme.colorScheme
    val (glyph, color) = when (status) {
        2 -> "!" to colors.error
        3 -> "✓✓" to colors.onSurfaceVariant.copy(0.7f)
        4 -> "✓✓" to colors.primary
        1 -> "✓" to colors.onSurfaceVariant.copy(0.7f)
        else -> return
    }
    Text(glyph, style = MaterialTheme.typography.labelMedium, color = color, maxLines = 1)
}

@Composable
private fun UnreadBadge(count: Int, muted: Boolean, colors: ColorScheme) {
    val bg = if (muted) colors.surfaceVariant else colors.primary
    val fg = if (muted) colors.onSurfaceVariant else colors.onPrimary
    Box(
        Modifier
            .heightIn(min = 20.dp)
            .defaultMinSize(minWidth = 20.dp)
            .clip(CircleShape)
            .background(bg)
            .padding(horizontal = 6.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            if (count > 99) "99+" else "$count",
            style = MaterialTheme.typography.labelMedium,
            fontWeight = FontWeight.Bold,
            color = fg,
            maxLines = 1,
        )
    }
}

@Composable
private fun DeleteChatDialog(name: String, onConfirm: () -> Unit, onDismiss: () -> Unit) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Delete chat") },
        text = { Text("Delete your chat with $name? This removes the contact and all messages on this device. This can't be undone.") },
        confirmButton = {
            TextButton(onClick = onConfirm) {
                Text("Delete", color = MaterialTheme.colorScheme.error)
            }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

private val OnlineDot = Color(0xFF34C759)
private val IdleDot = Color(0xFFF5A623)

/** Generic status colour for the avatar dot; null hides it (offline/unknown). */
private fun presenceColor(p: Presence?): Color? = when (p) {
    Presence.Online -> OnlineDot
    is Presence.Idle -> IdleDot
    else -> null
}
