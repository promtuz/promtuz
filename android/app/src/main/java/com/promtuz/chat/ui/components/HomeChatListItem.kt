package com.promtuz.chat.ui.components

import androidx.compose.ui.layout.boundsInRoot

import androidx.compose.ui.graphics.layer.drawLayer

import androidx.compose.ui.draw.drawWithContent

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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.promtuz.chat.domain.model.ChatSummary
import com.promtuz.chat.domain.model.mediaLabel
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.ui.text.parseMessageDate
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.theme.PromtuzColors
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.unit.sp
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
    onRequestDecision: (RequestDecision) -> Unit = {},
    privateChat: Boolean = false,
) {
        val c = Promtuz.colors
    val haptic = LocalHapticFeedback.current
    val unread = chat.unreadCount > 0

    var confirmDelete by remember { mutableStateOf(false) }
    var confirmClear by remember { mutableStateOf(false) }
    var confirmRequest by remember { mutableStateOf<RequestDecision?>(null) }
    val interaction = remember { MutableInteractionSource() }
    val rowCoord = remember { object { var c: LayoutCoordinates? = null } }
    val snapshot = androidx.compose.ui.graphics.rememberGraphicsLayer()

    val groups = if (chat.request) listOf(listOf(
        MenuAction("Block", icon = R.drawable.oi_user_slash, destructive = true) { confirmRequest = RequestDecision.Block },
        MenuAction("Delete", R.drawable.oi_trash, destructive = true) { confirmRequest = RequestDecision.Delete },
    )) else listOf(
        buildList {
            if (!privateChat && !chat.locked) add(MenuAction(if (pinned) "Unpin" else "Pin", if (pinned) R.drawable.oi_unpin else R.drawable.oi_pin) { onPin() })
            if (!privateChat && !chat.locked) add(MenuAction(if (muted) "Unmute" else "Mute", if (muted) R.drawable.oi_volume else R.drawable.oi_volume_off) { onMute() })
            if (unread) add(MenuAction("Mark read", R.drawable.oi_message_check) { onMarkRead() })
        },
        buildList {
            if (!chat.locked) add(MenuAction("Clear history", R.drawable.oi_broom) { confirmClear = true })
            add(MenuAction("Delete chat", R.drawable.oi_trash, destructive = true) { confirmDelete = true })
        },
    )

    Box(modifier) {
        Row(
            Modifier
                .fillMaxWidth()
                .onGloballyPositioned { rowCoord.c = it }
                .drawWithContent {
                    snapshot.record { this@drawWithContent.drawContent() }
                    drawLayer(snapshot)
                }
                .indication(interaction, PressHighlight)
                // A quick lift opens the chat, an early drag scrolls the list, and a hold
                // opens the menu for the same finger to drag to an item.
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
                                    val bounds = rowCoord.c?.takeIf { it.isAttached }?.boundsInRoot()
                                    menuState.open(HomeMenuAnchor(at, groups, bounds, snapshot))
                                    dragSelect(down, haptic, { menuState.hitIndex(rowCoord.c, it) }, { menuState.hovered = it }) {
                                        groups.flatten().getOrNull(it)?.onClick()
                                        menuState.close()
                                    }
                                }
                                else -> {} // SCROLL or GONE: the list scrolls
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
            if (chat.isGroup) GroupAvatar(title = chat.name, members = emptyList(), conversation = chat.conversationHex)
            else Avatar(chat.name, identityKey = chat.peerHex ?: chat.conversationHex, statusColor = presenceColor(presence), image = rememberAvatar(chat.peerHex))

            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(3.dp)) {
                Row(
                    Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        chat.name,
                        Modifier.weight(1f),
                        style = HomeNameStyle,
                        fontWeight = if (unread) FontWeight.Bold else null,
                        color = c.text,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    // [secret] placeholder until the glyph exists; same slot, size and tint as the others.
                    if (chat.isSecret) Text("[secret]", style = MaterialTheme.typography.labelSmall, color = c.muted)
                    if (privateChat) DrawableIcon(
                        R.drawable.oi_lock_chat_open, size = 15.dp, desc = "Unlocked chat",
                        tint = c.muted,
                    ) else if (chat.locked) DrawableIcon(
                        R.drawable.oi_lock_chat, size = 15.dp, desc = "Locked chat",
                        tint = c.muted,
                    )
                    if (muted) DrawableIcon(
                        R.drawable.i_volume_off, size = 15.dp, desc = "Muted",
                        tint = c.muted,
                    )
                    if (chat.timestampMs > 0) Row(
                        horizontalArrangement = Arrangement.spacedBy(3.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        if (chat.lastOutgoing && !typing) {
                            key(chat.conversationHex, chat.lastMessageId) {
                                MessageStatusIcon(
                                    status = SendStatus.from(chat.lastStatus),
                                    tint = c.muted,
                                    seenTint = c.accent,
                                )
                            }
                        }
                        Text(
                            parseMessageDate(LocalContext.current, chat.timestampMs),
                            style = HomeTimeStyle,
                            color = if (unread && !muted) c.accent else c.muted,
                        )
                    }
                }

                Row(
                    Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    val (line, lineColor) = statusLine(chat, typing, c)
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
                        style = HomePreviewStyle,
                        color = lineColor,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    if (pinned) DrawableIcon(
                        R.drawable.i_pin, size = 15.dp, desc = "Pinned",
                        tint = c.muted,
                    )
                    if (unread) UnreadBadge(chat.unreadCount, muted)
                }
            }
        }

        if (confirmDelete) DeleteChatDialog(
            chat = chat,
            onDelete = { confirmDelete = false; onDelete() },
            onLeaveAndDelete = { confirmDelete = false; onLeaveAndDelete() },
            onDismiss = { confirmDelete = false },
        )
        confirmRequest?.let { decision ->
            RequestConfirmDialog(decision, chat.name,
                onConfirm = { confirmRequest = null; onRequestDecision(decision) },
                onDismiss = { confirmRequest = null })
        }
        if (confirmClear) ClearHistoryDialog(
            name = chat.name,
            onConfirm = { confirmClear = false; onClearHistory() },
            onDismiss = { confirmClear = false },
        )
    }
}

private val HomeNameStyle = TextStyle(fontSize = 16.sp, lineHeight = 22.sp, fontWeight = FontWeight.Medium)
private val HomeTimeStyle = TextStyle(fontSize = 13.sp, lineHeight = 18.sp)
private val HomePreviewStyle = TextStyle(fontSize = 14.5.sp, lineHeight = 20.sp)

private fun statusLine(chat: ChatSummary, typing: Boolean, c: PromtuzColors): Pair<String, Color> = when {
    typing -> "typing…" to c.accent
    chat.locked -> "Locked chat" to c.muted
    chat.status == 0 && chat.lastMessageId == null -> "Waiting to connect…" to c.accent
    chat.status == 2 -> declineText(chat.rejectReason) to c.danger
    chat.lastDeleted -> "deleted message" to c.muted
    chat.lastPreview.isNullOrEmpty() && chat.lastMediaKind == 0 ->
        "No messages yet" to c.muted
    else -> {
        // A captionless picture or a voice note has no text of its own.
        val preview = chat.lastPreview.orEmpty().ifEmpty { mediaLabel(chat.lastMediaKind) }
        val text = if (chat.lastOutgoing) "You: $preview" else preview
        text to c.muted
    }
}

private fun declineText(reason: Int?): String = when (reason) {
    0 -> "Couldn’t start the chat. Try again."
    1 -> "Couldn't connect — their invite was already used"
    2 -> "Couldn't connect — they declined"
    else -> "Couldn't connect"
}

@Composable
private fun UnreadBadge(count: Int, muted: Boolean) {
    val c = Promtuz.colors
    val bg = if (muted) c.switchOff else c.accentFill
    val fg = if (muted) c.text else Color.White
    Box(
        Modifier
            .heightIn(min = 22.dp)
            .defaultMinSize(minWidth = 22.dp)
            .clip(RoundedCornerShape(8.dp))
            .background(bg)
            .padding(horizontal = 7.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            if (count > 99) "99+" else "$count",
            style = TextStyle(fontSize = 12.sp, lineHeight = 16.sp, fontWeight = FontWeight.Medium),
            color = fg,
            maxLines = 1,
        )
    }
}

enum class RequestDecision { Delete, Block }

@Composable
fun RequestConfirmDialog(decision: RequestDecision, name: String, onConfirm: () -> Unit, onDismiss: () -> Unit) {
    val block = decision == RequestDecision.Block
    AppAlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(if (block) "Block $name?" else "Delete request?") },
        text = {
            Text(
                if (block) "They can’t send you another request, and they won’t be told. Groups you share aren’t affected."
                else "The chat with $name is deleted. They won’t be told.",
            )
        },
        confirmButton = {
            TextButton(onClick = onConfirm) { Text(if (block) "Block" else "Delete", color = Promtuz.colors.danger) }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
fun DeleteChatDialog(
    chat: ChatSummary,
    onDelete: () -> Unit,
    onLeaveAndDelete: () -> Unit,
    onDismiss: () -> Unit,
) {
    val error = Promtuz.colors.danger
    if (chat.ownerIsStuck) {
        AppAlertDialog(
            onDismissRequest = onDismiss,
            title = { Text("You own this group") },
            text = {
                Text(
                    "Remove the other members before leaving. " +
                        "To delete only your copy, open Group info.",
                )
            },
            confirmButton = { TextButton(onClick = onDismiss) { Text("OK") } },
        )
        return
    }
    if (chat.isGroup && chat.commits) {
        AppAlertDialog(
            onDismissRequest = onDismiss,
            title = { Text("Delete this group chat?") },
            text = {
                Text(
                    "You’ll leave the group first, so someone else can manage it. " +
                        "This deletes the group’s messages from this device and can’t be undone.",
                )
            },
            confirmButton = { TextButton(onClick = onLeaveAndDelete) { Text("Leave and delete", color = error) } },
            dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
        )
        return
    }
    AppAlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(if (chat.isGroup) "Delete this group chat?" else "Delete chat") },
        text = {
            Text(
                when {
                    chat.isSecret ->
                        "This deletes the secret chat and its messages from this device. " +
                            "${chat.name} will see that you deleted it. This can't be undone."
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
                Text("Clear", color = Promtuz.colors.danger)
            }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

/** Null hides the avatar dot. */
@Composable
private fun presenceColor(p: Presence?): Color? = when (p) {
    Presence.Online -> Promtuz.colors.success
    is Presence.Idle -> Promtuz.colors.warn
    else -> null
}
