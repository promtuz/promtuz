package com.promtuz.chat.ui.screens

import android.content.ClipData
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
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
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.ui.components.TypingBubble
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
import com.promtuz.chat.ui.components.MessageMenuState
import com.promtuz.chat.ui.components.SelectionTopBar
import com.promtuz.chat.ui.components.SwipeToReply
import com.promtuz.chat.ui.components.rememberChatWallpaper
import dev.chrisbanes.haze.hazeSource
import dev.chrisbanes.haze.rememberHazeState
import kotlin.math.abs
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

private sealed interface ChatRow {
    data class Msg(val msg: UiMessage, val mergedTop: Boolean, val mergedBottom: Boolean) : ChatRow
    data class Frontier(val label: String) : ChatRow
    data object Typing : ChatRow
}

/** The list's row animator (matches the reference feel; change-animations stay off). */
private val RowPlacement = tween<IntOffset>(250, easing = CubicBezierEasing(0.19919f, 0.01064f, 0.27921f, 0.91025f))
private val RowFade = tween<Float>(200)

@Composable
fun ChatScreen(name: String, viewModel: ChatVM) {
    val messages by viewModel.messages.collectAsState()
    val typing by viewModel.typing.collectAsState()
    val appearance = LocalChatAppearance.current
    val layout = appearance.layout
    val mergeWindowMs = layout.mergeWindowSecs * 1000L
    val wallpaper = rememberChatWallpaper(appearance.wallpaper)
    val hazeState = rememberHazeState()

    val rows = remember(messages, mergeWindowMs, typing) { buildChatRows(messages, mergeWindowMs, typing) }
    val listState = rememberLazyListState()

    // Follow the conversation. Any change to the bottom row (new message, frontier
    // moving under it, typing bubble) re-evaluates; own sends always land us at the
    // bottom, incoming only pulls us when we're near it (scrolled-up reading holds).
    // Already exactly at the bottom → no scroll call at all: the placement
    // animations carry the motion, and a competing animateScroll just jitters.
    val bottomKey = rows.firstOrNull()?.let(::rowKey)
    val newestOutKey = (rows.firstOrNull { it is ChatRow.Msg } as? ChatRow.Msg)
        ?.msg?.takeIf { it.outgoing }?.key
    var lastOutKey by remember { mutableStateOf(newestOutKey) }
    LaunchedEffect(bottomKey) {
        val ownSend = newestOutKey != null && newestOutKey != lastOutKey
        lastOutKey = newestOutKey
        val atBottom = listState.firstVisibleItemIndex == 0 && listState.firstVisibleItemScrollOffset == 0
        val nearBottom = listState.firstVisibleItemIndex <= 3
        if (bottomKey != null && !atBottom && (nearBottom || ownSend)) {
            listState.animateScrollToItem(0)
        }
    }

    val menu = remember { MessageMenuState() }
    var confirmDelete by remember { mutableStateOf<UiMessage?>(null) }
    menu.onReact = { emoji ->
        menu.anchor?.let { viewModel.toggleReaction(it.msg, emoji) }
        menu.close()
    }

    val scope = rememberCoroutineScope()

    // Multi-select: keys of selected messages; non-empty = selection mode.
    var selected by remember { mutableStateOf(setOf<String>()) }
    var confirmBulkDelete by remember { mutableStateOf(false) }
    val selecting = selected.isNotEmpty()
    BackHandler(selecting) { selected = emptySet() }
    fun toggleSelect(key: String) {
        selected = if (key in selected) selected - key else selected + key
    }
    val clipboard = LocalClipboard.current
    fun copySelection() {
        val text = messages.asReversed()
            .filter { it.key in selected }
            .joinToString("\n") { (it.content as? MessageContent.Text)?.text.orEmpty() }
        scope.launch { clipboard.setClipEntry(ClipEntry(ClipData.newPlainText("messages", text))) }
    }

    // Tap on a reply's quote → glide to the quoted message and flash it.
    var highlightKey by remember { mutableStateOf<String?>(null) }
    fun jumpToQuoted(didHex: String) {
        val idx = rows.indexOfFirst { it is ChatRow.Msg && it.msg.dispatchIdHex == didHex }
        if (idx == -1) return
        val key = (rows[idx] as ChatRow.Msg).msg.key
        scope.launch {
            listState.animateScrollToItem(idx)
            highlightKey = key
            delay(1400)
            if (highlightKey == key) highlightKey = null
        }
    }

    Box {
        Scaffold(
            topBar = {
                AnimatedContent(selecting, label = "topbar") { inSelection ->
                    if (inSelection) SelectionTopBar(
                        count = selected.size,
                        haze = hazeState,
                        onClose = { selected = emptySet() },
                        onCopy = {
                            copySelection()
                            selected = emptySet()
                        },
                        onDelete = { confirmBulkDelete = true },
                    )
                    else ChatTopBar(name, viewModel, hazeState)
                }
            },
            bottomBar = { ChatBottomBar(viewModel, hazeState) },
        ) { padding ->
            // Wallpaper + list are the haze source; the translucent bars sample them. contentPadding
            // (not Modifier.padding) so messages scroll *under* the bars for the blur to have something.
            Box(Modifier.fillMaxSize().then(wallpaper).hazeSource(hazeState)) {
                LazyColumn(
                    Modifier.fillMaxSize(),
                    state = listState,
                    contentPadding = padding,
                    reverseLayout = true,
                ) {
                    items(rows, key = ::rowKey) { row ->
                        val animated = Modifier.animateItem(
                            fadeInSpec = RowFade,
                            placementSpec = RowPlacement,
                            fadeOutSpec = RowFade,
                        )
                        when (row) {
                            is ChatRow.Msg -> {
                                val gapAbove = if (row.mergedTop) layout.messageGap.dp else layout.groupGap.dp
                                val isSelected = row.msg.key in selected
                                val rowTint by animateColorAsState(
                                    if (isSelected) MaterialTheme.colorScheme.primary.copy(alpha = 0.14f)
                                    else if (highlightKey == row.msg.key) MaterialTheme.colorScheme.primary.copy(alpha = 0.22f)
                                    else Color.Transparent,
                                    label = "rowTint",
                                )
                                Box(animated.background(rowTint)) {
                                    SwipeToReply(
                                        enabled = !selecting && row.msg.dispatchIdHex != null && !row.msg.deleted,
                                        onReply = { viewModel.beginReply(row.msg) },
                                        Modifier
                                            .padding(top = gapAbove)
                                            .sendEnter(row.msg)
                                            // the context menu re-draws this row lifted; hide the original
                                            .graphicsLayer { alpha = if (menu.anchor?.msg?.key == row.msg.key) 0f else 1f },
                                    ) {
                                        MessageBubble(
                                            msg = row.msg,
                                            mergedTop = row.mergedTop,
                                            mergedBottom = row.mergedBottom,
                                            onLongPress = { bounds ->
                                                menu.open(MenuAnchor(row.msg, bounds, row.mergedTop, row.mergedBottom))
                                            },
                                            menuState = menu,
                                            onReactionTap = { viewModel.toggleReaction(row.msg, it) },
                                        )
                                    }
                                    // Selection mode: any tap on the row toggles; drawn above so it wins.
                                    if (selecting) Box(
                                        Modifier
                                            .matchParentSize()
                                            .clickable(
                                                interactionSource = remember { MutableInteractionSource() },
                                                indication = null,
                                            ) { toggleSelect(row.msg.key) },
                                    )
                                }
                            }
                            is ChatRow.Frontier -> FrontierMarker(row.label, animated)
                            is ChatRow.Typing -> TypingBubble(animated.padding(top = layout.groupGap.dp))
                        }
                    }
                }
            }
        }

        menu.anchor?.let { anchor ->
            MessageContextMenu(
                state = menu,
                quickReactions = QuickReactions,
                actionGroups = menuActionsFor(anchor.msg, viewModel, onDelete = { confirmDelete = it }) { menu.close() },
                onReact = { viewModel.toggleReaction(anchor.msg, it); menu.close() },
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

        if (confirmBulkDelete) AlertDialog(
            onDismissRequest = { confirmBulkDelete = false },
            title = { Text("Delete ${selected.size} messages?") },
            text = { Text("Your own messages are deleted for everyone; theirs only from this device.") },
            confirmButton = {
                TextButton({
                    messages.filter { it.key in selected }.forEach { m ->
                        m.dispatchIdHex?.let { viewModel.delete(it, forEveryone = m.outgoing) }
                    }
                    selected = emptySet()
                    confirmBulkDelete = false
                }) { Text("Delete", color = MaterialTheme.colorScheme.error) }
            },
            dismissButton = { TextButton({ confirmBulkDelete = false }) { Text("Cancel") } },
        )
    }
}

private val QuickReactions = listOf("❤️", "👍", "👎", "😂", "🔥", "😢")

/** Menu groups gated by ownership/state (destructive rides alone); every action closes via [close]. */
@Composable
private fun menuActionsFor(
    msg: UiMessage,
    viewModel: ChatVM,
    onDelete: (UiMessage) -> Unit,
    close: () -> Unit,
): List<List<MenuAction>> {
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    val main = buildList {
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
    }
    val destructive = buildList {
        if (msg.dispatchIdHex != null) add(MenuAction("Delete", R.drawable.i_delete, destructive = true) {
            onDelete(msg); close()
        })
    }
    return listOf(main, destructive).filter { it.isNotEmpty() }
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
private fun FrontierMarker(label: String, modifier: Modifier = Modifier) {
    val marker = LocalChatColors.current.marker
    Row(
        modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 3.dp),
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
    is ChatRow.Typing -> "typing"
}

/**
 * Own-send entrance: the freshly sent bubble rises from the composer (~0.4s spring)
 * while list placement slides everything else up. Runs only for rows first composed
 * while Pending, so scroll-back never replays it.
 */
@Composable
private fun Modifier.sendEnter(msg: UiMessage): Modifier {
    val fresh = remember(msg.key) { msg.outgoing && msg.status == SendStatus.Pending }
    if (!fresh) return this
    val progress = remember(msg.key) { Animatable(0f) }
    LaunchedEffect(msg.key) {
        progress.animateTo(1f, spring(dampingRatio = 0.8f, stiffness = Spring.StiffnessMediumLow))
    }
    return graphicsLayer {
        translationY = (1f - progress.value) * 46.dp.toPx()
        alpha = 0.4f + 0.6f * progress.value
    }
}

/**
 * Interleave message rows (with group merge flags) and status-frontier markers. A frontier answers
 * the one question the chat itself can't: "did it reach them / did they see it, given they haven't
 * responded?" — so it only shows when NOTHING incoming is newer than the tier's newest outgoing
 * message (their reply/receipt-by-response makes the marker redundant), and Sent has no marker at
 * all (pending already wears a spinner; everything else on screen is at least sent). A live typing
 * signal appends a [ChatRow.Typing] at the bottom (index 0 under reverseLayout).
 */
private fun buildChatRows(messages: List<UiMessage>, mergeWindowMs: Long, typing: Boolean): List<ChatRow> {
    val newestIncoming = messages.indexOfFirst { !it.outgoing }
    fun frontier(status: SendStatus): Int {
        val i = messages.indexOfFirst { it.outgoing && it.status == status }
        return if (i != -1 && (newestIncoming == -1 || i < newestIncoming)) i else -1
    }
    val seen = frontier(SendStatus.Read)
    val delivered = frontier(SendStatus.Delivered)

    val rows = ArrayList<ChatRow>(messages.size + 3)
    if (typing) rows.add(ChatRow.Typing)
    // A frontier line between two messages severs their group: the marker itself
    // is the visual break, so the bubbles on either side get full corners.
    fun frontierBetween(newer: Int, older: Int) = older == seen || older == delivered
    for (i in messages.indices) {
        when (i) {
            seen -> rows.add(ChatRow.Frontier("Seen"))
            delivered -> rows.add(ChatRow.Frontier("Delivered"))
        }
        val m = messages[i]
        val older = messages.getOrNull(i + 1)
        val newer = messages.getOrNull(i - 1)
        val mergedTop = older != null && sameGroup(m, older, mergeWindowMs) && !frontierBetween(i, i + 1)
        val mergedBottom = newer != null && sameGroup(m, newer, mergeWindowMs) && !frontierBetween(i - 1, i)
        rows.add(ChatRow.Msg(m, mergedTop, mergedBottom))
    }
    return rows
}

private fun sameGroup(a: UiMessage, b: UiMessage, windowMs: Long): Boolean =
    a.outgoing == b.outgoing && abs(a.timestampMs - b.timestampMs) <= windowMs
