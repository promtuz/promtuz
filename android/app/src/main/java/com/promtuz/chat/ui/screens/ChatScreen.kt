package com.promtuz.chat.ui.screens

import android.content.ClipData
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.ui.appearance.LocalChatAppearance
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.components.ChatBottomBar
import com.promtuz.chat.ui.components.ChatTopBar
import com.promtuz.chat.ui.components.DashedHorizontalDivider
import com.promtuz.chat.ui.components.MenuAction
import com.promtuz.chat.ui.components.MenuAnchor
import com.promtuz.chat.ui.components.MessageBubble
import com.promtuz.chat.ui.components.MessageContextMenu
import com.promtuz.chat.ui.components.SwipeToReply
import com.promtuz.chat.ui.components.rememberChatWallpaper
import dev.chrisbanes.haze.hazeSource
import dev.chrisbanes.haze.rememberHazeState
import kotlin.math.abs
import kotlinx.coroutines.launch

private sealed interface ChatRow {
    data class Msg(val msg: UiMessage, val mergedTop: Boolean, val mergedBottom: Boolean) : ChatRow
    data class Frontier(val label: String) : ChatRow
}

@Composable
fun ChatScreen(name: String, viewModel: ChatVM) {
    val messages by viewModel.messages.collectAsState()
    val appearance = LocalChatAppearance.current
    val layout = appearance.layout
    val mergeWindowMs = layout.mergeWindowSecs * 1000L
    val wallpaper = rememberChatWallpaper(appearance.wallpaper)
    val hazeState = rememberHazeState()

    val rows = remember(messages, mergeWindowMs) { buildChatRows(messages, mergeWindowMs) }

    var menu by remember { mutableStateOf<MenuAnchor?>(null) }
    var confirmDelete by remember { mutableStateOf<UiMessage?>(null) }

    Box {
        Scaffold(
            topBar = { ChatTopBar(name, viewModel, hazeState) },
            bottomBar = { ChatBottomBar(viewModel, hazeState) },
        ) { padding ->
            // Wallpaper + list are the haze source; the translucent bars sample them. contentPadding
            // (not Modifier.padding) so messages scroll *under* the bars for the blur to have something.
            Box(Modifier.fillMaxSize().then(wallpaper).hazeSource(hazeState)) {
                LazyColumn(
                    Modifier.fillMaxSize(),
                    contentPadding = padding,
                    reverseLayout = true,
                ) {
                    items(rows, key = ::rowKey) { row ->
                        when (row) {
                            is ChatRow.Msg -> {
                                val gapAbove = if (row.mergedTop) layout.messageGap.dp else layout.groupGap.dp
                                SwipeToReply(
                                    enabled = row.msg.dispatchIdHex != null && !row.msg.deleted,
                                    onReply = { viewModel.beginReply(row.msg) },
                                    Modifier
                                        .padding(top = gapAbove)
                                        // the context menu re-draws this row lifted; hide the original
                                        .graphicsLayer { alpha = if (menu?.msg?.key == row.msg.key) 0f else 1f },
                                ) {
                                    MessageBubble(
                                        msg = row.msg,
                                        mergedTop = row.mergedTop,
                                        mergedBottom = row.mergedBottom,
                                        onLongPress = { bounds ->
                                            menu = MenuAnchor(row.msg, bounds, row.mergedTop, row.mergedBottom)
                                        },
                                        onReactionTap = { viewModel.toggleReaction(row.msg, it) },
                                    )
                                }
                            }
                            is ChatRow.Frontier -> FrontierMarker(row.label)
                        }
                    }
                }
            }
        }

        menu?.let { anchor ->
            MessageContextMenu(
                anchor = anchor,
                quickReactions = QuickReactions,
                actions = menuActionsFor(anchor.msg, viewModel, onDelete = { confirmDelete = it }) { menu = null },
                onReact = { viewModel.toggleReaction(anchor.msg, it); menu = null },
                onDismiss = { menu = null },
            )
        }

        confirmDelete?.let { msg ->
            DeleteConfirmDialog(
                msg = msg,
                onConfirm = {
                    msg.dispatchIdHex?.let { viewModel.delete(it, forEveryone = msg.outgoing) }
                    confirmDelete = null
                },
                onDismiss = { confirmDelete = null },
            )
        }
    }
}

private val QuickReactions = listOf("❤️", "👍", "👎", "😂", "🔥", "😢")

/** Menu items gated by ownership/state; every action closes the menu via [close]. */
@Composable
private fun menuActionsFor(
    msg: UiMessage,
    viewModel: ChatVM,
    onDelete: (UiMessage) -> Unit,
    close: () -> Unit,
): List<MenuAction> {
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    return buildList {
        val actionable = msg.dispatchIdHex != null && !msg.deleted
        if (actionable) add(MenuAction("Reply", R.drawable.i_reply) {
            viewModel.beginReply(msg); close()
        })
        if (!msg.deleted) add(MenuAction("Copy", R.drawable.i_copy) {
            val text = (msg.content as? MessageContent.Text)?.text.orEmpty()
            scope.launch {
                clipboard.setClipEntry(ClipEntry(ClipData.newPlainText("message", text)))
            }
            close()
        })
        if (actionable && msg.outgoing) add(MenuAction("Edit", R.drawable.i_edit) {
            viewModel.beginEdit(msg); close()
        })
        if (msg.dispatchIdHex != null) add(MenuAction("Delete", R.drawable.i_delete, destructive = true) {
            onDelete(msg); close()
        })
    }
}

@Composable
private fun DeleteConfirmDialog(msg: UiMessage, onConfirm: () -> Unit, onDismiss: () -> Unit) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Delete message?") },
        text = {
            Text(
                if (msg.outgoing) "It will be deleted for everyone in this chat."
                else "It will be removed from this device."
            )
        },
        confirmButton = {
            TextButton(onConfirm) { Text("Delete", color = MaterialTheme.colorScheme.error) }
        },
        dismissButton = { TextButton(onDismiss) { Text("Cancel") } },
    )
}

/**
 * A subtle right-aligned frontier line — "everything above is [label]". Deliberately short and
 * right-of-column so it never reads as a (centered) day separator. Delivery state shows here once
 * per tier, not per bubble (receipts are a high-water-mark).
 */
@Composable
private fun FrontierMarker(label: String) {
    val marker = LocalChatColors.current.marker
    Row(
        Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 3.dp),
        horizontalArrangement = Arrangement.End,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        DashedHorizontalDivider(Modifier.weight(1f), color = marker.copy(alpha = 0.25f))
        Text(
            label.uppercase(),
            style = MaterialTheme.typography.labelSmall,
            color = marker.copy(alpha = 0.5f),
            modifier = Modifier.padding(start = 6.dp),
        )
    }
}

private fun rowKey(row: ChatRow): Any = when (row) {
    is ChatRow.Msg -> row.msg.key
    is ChatRow.Frontier -> "frontier:${row.label}"
}

/**
 * Interleave message rows (with group merge flags) and status-frontier markers. Each frontier is the
 * newest outgoing message of its tier (lowest index in the newest-first list); the marker sits just
 * below it, so "above the line" is that status or better. Absent tiers produce no marker.
 */
private fun buildChatRows(messages: List<UiMessage>, mergeWindowMs: Long): List<ChatRow> {
    fun frontier(status: SendStatus) = messages.indexOfFirst { it.outgoing && it.status == status }
    val seen = frontier(SendStatus.Read)
    val delivered = frontier(SendStatus.Delivered)
    val sent = frontier(SendStatus.Sent)

    val rows = ArrayList<ChatRow>(messages.size + 3)
    for (i in messages.indices) {
        when (i) {
            seen -> rows.add(ChatRow.Frontier("Seen"))
            delivered -> rows.add(ChatRow.Frontier("Delivered"))
            sent -> rows.add(ChatRow.Frontier("Sent"))
        }
        val m = messages[i]
        val older = messages.getOrNull(i + 1)
        val newer = messages.getOrNull(i - 1)
        val mergedTop = older != null && sameGroup(m, older, mergeWindowMs)
        val mergedBottom = newer != null && sameGroup(m, newer, mergeWindowMs)
        rows.add(ChatRow.Msg(m, mergedTop, mergedBottom))
    }
    return rows
}

private fun sameGroup(a: UiMessage, b: UiMessage, windowMs: Long): Boolean =
    a.outgoing == b.outgoing && abs(a.timestampMs - b.timestampMs) <= windowMs
