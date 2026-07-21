package com.promtuz.chat.ui.screens

import android.content.ClipData
import android.content.Context
import android.content.Intent
import java.io.File
import java.net.URLConnection
import androidx.core.content.FileProvider
import androidx.compose.ui.platform.LocalContext
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.spring
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.AlertDialog
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
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.ui.appearance.DoubleTapAction
import com.promtuz.chat.ui.appearance.LocalChatAppearance
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.components.ChatBottomBar
import com.promtuz.chat.ui.components.ChatTopBar
import com.promtuz.chat.ui.components.DashedHorizontalDivider
import com.promtuz.chat.ui.components.NotificationPrimer
import com.promtuz.chat.ui.components.MenuAction
import com.promtuz.chat.ui.components.MenuAnchor
import com.promtuz.chat.ui.components.MessageBubble
import com.promtuz.chat.ui.components.MessageContextMenu
import com.promtuz.chat.ui.components.MessageMenuState
import com.promtuz.chat.ui.components.SwipeToReply
import com.promtuz.chat.ui.components.TypingBubble
import com.promtuz.chat.ui.components.rememberChatWallpaper
import com.promtuz.chat.ui.stage.MessageStage
import com.promtuz.chat.ui.stage.rememberMessageStageState
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

// Best-effort "open" for a finished download: hand the file to the system via the
// app's FileProvider. Silently no-ops if the path isn't under a shared root or no
// app can view the type — the ready state on the card is the real signal.
private fun openAttachment(context: Context, path: String) {
    runCatching {
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", File(path))
        val mime = URLConnection.guessContentTypeFromName(path) ?: "*/*"
        context.startActivity(
            Intent(Intent.ACTION_VIEW)
                .setDataAndType(uri, mime)
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        )
    }
}

@Composable
fun ChatScreen(name: String, viewModel: ChatVM) {
    val messages by viewModel.messages.collectAsState()
    val typing by viewModel.typing.collectAsState()
    val appearance = LocalChatAppearance.current
    val layout = appearance.layout
    val mergeWindowMs = layout.mergeWindowSecs * 1000L
    val wallpaper = rememberChatWallpaper(appearance.wallpaper)
    val hazeState = rememberHazeState()
    val scope = rememberCoroutineScope()
    val context = LocalContext.current

    // High-intent moment to ask for notifications: they're in a conversation. One-shot, self-gated.
    NotificationPrimer()

    // Messages paint the moment they load — no nav-slide gate, no cascade. The stage
    // is windowed (only the visible band is measured), so the full loaded window sits
    // in the list free off-screen; older pages arrive via onNearTop on scroll.
    val rows = remember(messages, mergeWindowMs, typing) { buildChatRows(messages, mergeWindowMs, typing) }
    val stage = rememberMessageStageState()

    // Own sends always land us at the bottom; incoming near the bottom is the
    // stage's built-in follow, and scrolled-up reading holds.
    val newestOutKey = (rows.firstOrNull { it is ChatRow.Msg } as? ChatRow.Msg)
        ?.msg?.takeIf { it.outgoing }?.key
    var lastOutKey by remember { mutableStateOf(newestOutKey) }
    LaunchedEffect(newestOutKey) {
        val ownSend = newestOutKey != null && newestOutKey != lastOutKey
        lastOutKey = newestOutKey
        if (ownSend) stage.scrollToBottom()
    }

    val menu = remember { MessageMenuState() }
    var confirmDelete by remember { mutableStateOf<UiMessage?>(null) }
    menu.onReact = { emoji ->
        menu.anchor?.let { viewModel.toggleReaction(it.msg, emoji) }
        menu.close()
    }

    // Suspended row = the anchor: the stage derives scroll so it never moves while
    // the menu is up; everything else grows away from it.
    LaunchedEffect(menu.anchor) {
        val anchor = menu.anchor
        if (anchor != null) stage.pin(anchor.msg.key, anchor.bounds.bottom)
        else stage.unpin()
    }

    // Tap on a reply's quote → glide to the quoted message and flash it.
    var highlightKey by remember { mutableStateOf<String?>(null) }
    fun jumpToQuoted(didHex: String) {
        val target = (rows.firstOrNull { it is ChatRow.Msg && it.msg.dispatchIdHex == didHex } as? ChatRow.Msg)
            ?: return
        scope.launch {
            stage.scrollToKey(target.msg.key)
            highlightKey = target.msg.key
            delay(1400)
            if (highlightKey == target.msg.key) highlightKey = null
        }
    }

    Box {
        Scaffold(
            topBar = { ChatTopBar(name, viewModel, hazeState) },
            bottomBar = { ChatBottomBar(viewModel, hazeState) },
        ) { padding ->
        // Wallpaper + stage are the haze source; the translucent bars sample them.
        // contentPadding (not an outer padding) so messages draw under the bars.
        Box(
            Modifier
                .fillMaxSize()
                .then(wallpaper)
                .hazeSource(hazeState),
        ) {
            val handoff by viewModel.typingHandoff.collectAsState()
            MessageStage(
                rows = rows,
                key = ::rowKey,
                state = stage,
                contentPadding = padding,
                modifier = Modifier.fillMaxSize(),
                // The incoming message that ended a live typing signal inherits the
                // typing bubble: same spot, its height, dots out / text in.
                morphFrom = { r -> if (r is ChatRow.Msg && r.msg.key == handoff) "typing" else null },
                transformOrigin = { r ->
                    when (r) {
                        is ChatRow.Msg -> TransformOrigin(if (r.msg.outgoing) 1f else 0f, 1f)
                        is ChatRow.Typing -> TransformOrigin(0f, 1f)
                        else -> TransformOrigin(0.5f, 1f)
                    }
                },
                onNearTop = viewModel::loadOlder,
            ) { chatRow ->
                when (chatRow) {
                    is ChatRow.Msg -> {
                        val gapAbove = if (chatRow.mergedTop) layout.messageGap.dp else layout.groupGap.dp
                        val highlight by animateColorAsState(
                            if (highlightKey == chatRow.msg.key)
                                MaterialTheme.colorScheme.primary.copy(alpha = 0.22f)
                            else Color.Transparent,
                            label = "highlight",
                        )
                        Box(Modifier.background(highlight)) {
                            SwipeToReply(
                                enabled = chatRow.msg.dispatchIdHex != null && !chatRow.msg.deleted,
                                onReply = { viewModel.beginReply(chatRow.msg) },
                                Modifier
                                    .padding(top = gapAbove)
                                    .sendEnter(chatRow.msg)
                                    // the context menu re-draws this row lifted; hide the original
                                    .graphicsLayer { alpha = if (menu.anchor?.msg?.key == chatRow.msg.key) 0f else 1f },
                            ) {
                                val actionable = chatRow.msg.dispatchIdHex != null && !chatRow.msg.deleted
                                val interaction = appearance.interaction
                                MessageBubble(
                                    msg = chatRow.msg,
                                    mergedTop = chatRow.mergedTop,
                                    mergedBottom = chatRow.mergedBottom,
                                    onLongPress = { bounds ->
                                        menu.open(MenuAnchor(chatRow.msg, bounds, chatRow.mergedTop, chatRow.mergedBottom))
                                    },
                                    menuState = menu,
                                    onReactionTap = { viewModel.toggleReaction(chatRow.msg, it) },
                                    onQuoteClick = ::jumpToQuoted,
                                    onDownload = viewModel::download,
                                    onOpen = { openAttachment(context, it) },
                                    peerName = name,
                                    onDoubleTap = when {
                                        !actionable -> null
                                        interaction.doubleTapAction == DoubleTapAction.React ->
                                            { { viewModel.toggleReaction(chatRow.msg, interaction.doubleTapEmoji) } }
                                        interaction.doubleTapAction == DoubleTapAction.Reply ->
                                            { { viewModel.beginReply(chatRow.msg) } }
                                        interaction.doubleTapAction == DoubleTapAction.Edit && chatRow.msg.outgoing ->
                                            { { viewModel.beginEdit(chatRow.msg) } }
                                        else -> null
                                    },
                                )
                            }
                        }
                    }
                    is ChatRow.Frontier -> FrontierMarker(chatRow.label)
                    is ChatRow.Typing -> TypingBubble(Modifier.padding(top = layout.groupGap.dp))
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
 * Own-send entrance: the freshly sent bubble rises from the composer while the
 * stage opens room for it. Runs only for rows first composed while Pending, so
 * scroll-back never replays it.
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
 * signal appends a [ChatRow.Typing] at the bottom (index 0). A frontier line between two messages
 * severs their merge group: the marker itself is the visual break.
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
    fun frontierBetween(older: Int) = older == seen || older == delivered
    for (i in messages.indices) {
        when (i) {
            seen -> rows.add(ChatRow.Frontier("Seen"))
            delivered -> rows.add(ChatRow.Frontier("Delivered"))
        }
        val m = messages[i]
        val older = messages.getOrNull(i + 1)
        val newer = messages.getOrNull(i - 1)
        val mergedTop = older != null && sameGroup(m, older, mergeWindowMs) && !frontierBetween(i + 1)
        val mergedBottom = newer != null && sameGroup(m, newer, mergeWindowMs) && !frontierBetween(i)
        rows.add(ChatRow.Msg(m, mergedTop, mergedBottom))
    }
    return rows
}

private fun sameGroup(a: UiMessage, b: UiMessage, windowMs: Long): Boolean =
    a.outgoing == b.outgoing && abs(a.timestampMs - b.timestampMs) <= windowMs
