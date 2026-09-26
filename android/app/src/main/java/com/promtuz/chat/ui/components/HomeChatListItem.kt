package com.promtuz.chat.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.ui.layout.ContentScale
import com.promtuz.chat.utils.media.rememberMessagePreview
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
import androidx.compose.runtime.key
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
import com.promtuz.chat.domain.model.mediaLabel
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.utils.common.parseMessageDate
import kotlinx.coroutines.withTimeoutOrNull
import com.promtuz.chat.utils.media.rememberAvatar

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
    onClearHistory: () -> Unit,
    onDelete: () -> Unit,
    onLeaveAndDelete: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val type = MaterialTheme.typography
    val colors = MaterialTheme.colorScheme
    val haptic = LocalHapticFeedback.current
    val unread = chat.unreadCount > 0

    var confirmDelete by remember { mutableStateOf(false) }
    var confirmClear by remember { mutableStateOf(false) }
    val interaction = remember { MutableInteractionSource() }
    val rowCoord = remember { object { var c: LayoutCoordinates? = null } }

    val groups = listOf(
        buildList {
            add(MenuAction(if (pinned) "Unpin" else "Pin", if (pinned) R.drawable.oi_unpin else R.drawable.oi_pin) { onPin() })
            add(MenuAction(if (muted) "Unmute" else "Mute", if (muted) R.drawable.oi_volume else R.drawable.oi_volume_off) { onMute() })
            if (unread) add(MenuAction("Mark read", R.drawable.oi_message_check) { onMarkRead() })
        },
        listOf(
            MenuAction("Clear history", R.drawable.oi_broom) { confirmClear = true },
            MenuAction("Delete chat", R.drawable.oi_trash, destructive = true) { confirmDelete = true },
        ),
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
                .pointerInput(chat.conversationHex, pinned, muted, unread) {
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
            if (chat.isGroup) GroupAvatar(title = chat.name, members = emptyList())
            else Avatar(chat.name, statusColor = presenceColor(presence), image = rememberAvatar(chat.peerHex))

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
                        if (chat.lastOutgoing && !typing) {
                            key(chat.conversationHex, chat.lastMessageId) {
                                MessageStatusIcon(
                                    status = SendStatus.from(chat.lastStatus),
                                    tint = colors.onSurfaceVariant.copy(0.7f),
                                    seenTint = colors.primary,
                                )
                            }
                        }
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
                    if (!typing && chat.status == 1 && !chat.lastDeleted && chat.lastMediaKind in listOf(1, 2, 4)) {
                        chat.lastDispatchId?.let { did ->
                            val preview = rememberMessagePreview(chat.conversationHex, did, retry = chat.lastMediaKind == 4)
                            if (preview != null) Image(
                                preview, contentDescription = null,
                                modifier = Modifier.size(22.dp).clip(RoundedCornerShape(4.dp)),
                                contentScale = if (chat.lastMediaKind == 4) ContentScale.Fit else ContentScale.Crop,
                            )
                        }
                    }
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
            chat = chat,
            onDelete = { confirmDelete = false; onDelete() },
            onLeaveAndDelete = { confirmDelete = false; onLeaveAndDelete() },
            onDismiss = { confirmDelete = false },
        )
        if (confirmClear) ClearHistoryDialog(
            name = chat.name,
            onConfirm = { confirmClear = false; onClearHistory() },
            onDismiss = { confirmClear = false },
        )
    }
}

/** The preview/status line: live typing beats pairing state beats last message. */
private fun statusLine(chat: ChatSummary, typing: Boolean, colors: ColorScheme): Pair<String, Color> = when {
    typing -> "typing…" to colors.primary
    chat.status == 0 -> "Waiting to connect…" to colors.primary.copy(0.8f)
    chat.status == 2 -> declineText(chat.rejectReason) to colors.error.copy(0.85f)
    chat.lastDeleted -> "deleted message" to colors.onSurfaceVariant.copy(0.6f)
    chat.lastPreview.isNullOrEmpty() && chat.lastMediaKind == 0 ->
        "No messages yet" to colors.onSurfaceVariant.copy(0.6f)
    else -> {
        // A captionless picture or a voice note has no text of its own.
        val preview = chat.lastPreview.orEmpty().ifEmpty { mediaLabel(chat.lastMediaKind) }
        val text = if (chat.lastOutgoing) "You: $preview" else preview
        val col = if (chat.unreadCount > 0) colors.onSurface.copy(0.9f) else colors.onSurfaceVariant.copy(0.7f)
        text to col
    }
}

/** "Couldn't connect" plus the decline reason (DECLINE_* code), when known. */
private fun declineText(reason: Int?): String = when (reason) {
    0 -> "Couldn’t start the chat. Try again."
    1 -> "Couldn't connect — their invite was already used"
    2 -> "Couldn't connect — they declined"
    else -> "Couldn't connect"
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

/**
 * Deleting a group and leaving one are different acts, and the dialog says so
 * rather than quietly picking. Delete is silent — the group carries on without
 * you and nobody there learns you have gone — so offering only that would be a
 * trapdoor for someone who meant to say goodbye.
 */
@Composable
fun DeleteChatDialog(
    chat: ChatSummary,
    onDelete: () -> Unit,
    onLeaveAndDelete: () -> Unit,
    onDismiss: () -> Unit,
) {
    val error = MaterialTheme.colorScheme.error
    if (chat.ownerIsStuck) {
        AppAlertDialog(
            onDismissRequest = onDismiss,
            title = { Text("You’re the group admin") },
            text = {
                Text(
                    "You’re the only admin. Remove the other members before leaving. " +
                        "To delete only your copy, open Group info.",
                )
            },
            confirmButton = { TextButton(onClick = onDismiss) { Text("OK") } },
        )
        return
    }
    AppAlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(if (chat.isGroup) "Delete this group chat?" else "Delete chat") },
        text = {
            Text(
                when {
                    !chat.isGroup ->
                        "Delete your chat with ${chat.name}? This removes the contact and " +
                            "all messages on this device. This can't be undone."
                    chat.amMember ->
                        "This deletes your messages and group access from this device. " +
                            "Other members won’t be notified. Choose Leave and delete to leave the group too. " +
                            "This can’t be undone."
                    else ->
                        "This deletes the group’s messages from this device. This can’t be undone."
                },
            )
        },
        confirmButton = {
            TextButton(onClick = onDelete) { Text("Delete", color = error) }
        },
        dismissButton = {
            Row {
                TextButton(onClick = onDismiss) { Text("Cancel") }
                if (chat.canLeave) {
                    TextButton(onClick = onLeaveAndDelete) {
                        Text("Leave and delete", color = error)
                    }
                }
            }
        },
    )
}

/**
 * Clearing is not leaving and not deleting — the chat stays, and so does
 * everyone else's copy. Confirmed all the same: the messages don't come back.
 */
@Composable
fun ClearHistoryDialog(name: String, onConfirm: () -> Unit, onDismiss: () -> Unit) {
    AppAlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Clear history?") },
        text = {
            Text(
                "Delete every message in \"$name\" from this device. The chat itself " +
                    "stays, and nobody else loses anything. This can't be undone.",
            )
        },
        confirmButton = {
            TextButton(onClick = onConfirm) {
                Text("Clear", color = MaterialTheme.colorScheme.error)
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
