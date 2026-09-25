package com.promtuz.chat.ui.screens

import android.content.ClipData
import android.content.Context
import android.content.Intent
import java.io.File
import java.net.URLConnection
import androidx.core.content.FileProvider
import androidx.compose.ui.platform.LocalContext
import androidx.compose.animation.animateColorAsState
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
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
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.LayoutDirection
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.StickerRef
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.presentation.viewmodel.StickersVM
import com.promtuz.chat.ui.components.StickerPackSheet
import com.promtuz.chat.ui.media.MediaViewer
import com.promtuz.chat.ui.media.LocalMediaClip
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.layout.boundsInWindow
import androidx.compose.ui.layout.onGloballyPositioned
import com.promtuz.chat.ui.media.chatMediaItems
import org.koin.androidx.compose.koinViewModel
import org.koin.compose.koinInject
import com.promtuz.chat.ui.appearance.DoubleTapAction
import com.promtuz.chat.ui.appearance.LocalChatAppearance
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.components.ChatBottomBar
import com.promtuz.chat.ui.components.ChatTopBar
import com.promtuz.chat.ui.components.ChatDateDivider
import com.promtuz.chat.ui.components.rememberChatCalendar
import com.promtuz.chat.ui.components.ComposerMetrics
import com.promtuz.chat.ui.components.rememberComposerMetrics
import com.promtuz.chat.ui.components.NotificationPrimer
import com.promtuz.chat.ui.components.MenuAction
import com.promtuz.chat.ui.components.MenuAnchor
import com.promtuz.chat.ui.components.MessageBubble
import com.promtuz.chat.ui.components.MessageContextMenu
import com.promtuz.chat.ui.components.MessageMenuState
import com.promtuz.chat.ui.components.SwipeToReply
import com.promtuz.chat.ui.theme.bottomScrim
import com.promtuz.chat.ui.components.TypingBubble
import com.promtuz.chat.ui.components.rememberChatWallpaper
import com.promtuz.chat.ui.stage.MessageStage
import com.promtuz.chat.ui.stage.rememberMessageStageState
import dev.chrisbanes.haze.hazeSource
import dev.chrisbanes.haze.rememberHazeState
import androidx.compose.ui.text.style.TextAlign
import com.promtuz.chat.ui.components.BubbleTextLayouts
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

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
fun ChatScreen(routeName: String, viewModel: ChatVM) {
    androidx.lifecycle.compose.LifecycleResumeEffect(viewModel) {
        viewModel.setChatForeground(true)
        onPauseOrDispose { viewModel.setChatForeground(false) }
    }
    // Capture once: Scaffold must receive rows and load state from the same snapshot.
    val messageSnapshot = viewModel.messages.collectAsState().value
    val messages = messageSnapshot.orEmpty()
    val typingMembers by viewModel.typingBubbleMembers.collectAsState()
    val typing = typingMembers.isNotEmpty()
    val isGroup by viewModel.isGroup.collectAsState()
    // A group can be renamed while it's open, and the route's name is a
    // snapshot from when it was pushed. A 1:1 has no title of its own, so it
    // keeps the contact name the route carried.
    val title by viewModel.title.collectAsState()
    val name = title.ifEmpty { routeName }
    val appearance = LocalChatAppearance.current
    val layout = appearance.layout
    val mergeWindowMs = layout.mergeWindowSecs * 1000L
    val wallpaper = rememberChatWallpaper(appearance.wallpaper)
    val hazeState = rememberHazeState()
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    val calendar = rememberChatCalendar()
    var selectedDate by remember { mutableStateOf<java.time.LocalDate?>(null) }

    // High-intent moment to ask for notifications: they're in a conversation. One-shot, self-gated.
    NotificationPrimer()

    // Messages paint the moment they load — no nav-slide gate, no cascade. The stage
    // is windowed (only the visible band is measured), so the full loaded window sits
    // in the list free off-screen; older pages arrive via onNearTop on scroll.
    var groupingNow by remember { mutableStateOf(System.currentTimeMillis()) }
    LaunchedEffect(messages.firstOrNull()?.key, typingMembers, mergeWindowMs, calendar) {
        groupingNow = System.currentTimeMillis()
        val remaining = (messages.firstOrNull()?.timestampMs ?: 0L) + mergeWindowMs - groupingNow
        if (typing && remaining >= 0) {
            delay(remaining + 1)
            groupingNow = System.currentTimeMillis()
        }
    }
    val rows = remember(messages, mergeWindowMs, typing, typingMembers, isGroup, groupingNow, calendar) {
        buildChatRows(messages, mergeWindowMs, typing, typingMembers, isGroup, groupingNow, calendar.zone)
    }
    val stage = rememberMessageStageState()
    val metrics = rememberComposerMetrics()

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
    // A tapped sticker opens the pack it came from.
    var packSheet by remember { mutableStateOf<StickerRef?>(null) }
    val appVM = koinInject<AppVM>()
    val stickersVM = koinViewModel<StickersVM>()
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
    // A search hit names a message that may still be loading in: hold the
    // id until its row is on the stage, then make the same glide a quote does.
    var pendingJump by remember { mutableStateOf<String?>(null) }
    LaunchedEffect(Unit) { viewModel.jump.collect { pendingJump = it } }
    LaunchedEffect(pendingJump, rows) {
        val did = pendingJump ?: return@LaunchedEffect
        if (rows.any { rowKey(it) == did }) {
            pendingJump = null
            // Clearing the pending key restarts this effect; the actual glide
            // must outlive that restart and reactive message-list updates.
            scope.launch {
                stage.scrollToKey(did)
                highlightKey = did
                delay(1400)
                if (highlightKey == did) highlightKey = null
            }
        }
    }

    Box {
        Scaffold(
            topBar = { ChatTopBar(name, viewModel, hazeState) },
            bottomBar = {
                ChatBottomBar(
                    viewModel, hazeState, metrics, onJumpTo = ::jumpToQuoted,
                    onManageStickers = { appVM.navigator.push(Routes.Stickers) },
                    onCreateStickerPack = { appVM.navigator.push(Routes.NewStickerPack()) },
                )
            },
        ) { padding ->
        // Wallpaper + stage are the haze source; the translucent bars sample them.
        // contentPadding (not an outer padding) so messages draw under the bars.
        val stageCoords = remember { arrayOfNulls<androidx.compose.ui.layout.LayoutCoordinates>(1) }
        Box(
            Modifier
                .fillMaxSize()
                .then(wallpaper)
                .hazeSource(hazeState)
                .onGloballyPositioned { stageCoords[0] = it },
        ) {
            val handoff by viewModel.typingHandoff.collectAsState()
            val density = LocalDensity.current
            // Scaffold's inset lands a frame behind the composer's reveal, so the bar
            // publishes its own footprint instead and the stage samples it during
            // measure — one value, one pass, no drift between bar and messages.
            val stagePadding = remember(padding, metrics, density) {
                ComposerPadding(padding.calculateTopPadding(), metrics, density)
            }
            // The media viewer flies pictures out of, and back into, only the strip
            // between the bars, so nothing crisp is ever drawn over a bar's blur.
            val topPadPx = with(density) { stagePadding.calculateTopPadding().toPx() }
            val bottomPadPx = with(density) { stagePadding.calculateBottomPadding().toPx() }
            CompositionLocalProvider(LocalMediaClip provides {
                stageCoords[0]?.takeIf { it.isAttached }?.boundsInWindow()?.let { r ->
                    Rect(r.left, r.top + topPadPx, r.right, r.bottom - bottomPadPx)
                }
            }) {
            MessageStage(
                rows = rows,
                historyLoaded = messageSnapshot != null,
                key = ::rowKey,
                state = stage,
                contentPadding = stagePadding,
                pushBottom = { with(density) { metrics.pushPx.toDp() } },
                modifier = Modifier.fillMaxSize(),
                animateOnInitialFill = { it is ChatRow.Typing },
                // A real typing handoff or a send prepared by this composer owns its
                // entrance even if it happens while the initial query is returning.
                morphFrom = { r -> if (r is ChatRow.Msg && r.msg.key == handoff) "typing" else null },
                entranceClock = { r ->
                    if (r is ChatRow.Msg && r.msg.outgoing) {
                        metrics.sendTransition?.claim()
                    } else null
                },
                enterFromBelow = { r -> if (r is ChatRow.Msg) metrics.composerPx.toFloat() else 0f },
                horizontalPivotInset = 12.dp,
                stickyHeader = { it is ChatRow.Date },
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
                        val gapAbove by androidx.compose.animation.core.animateDpAsState(
                            if (chatRow.mergedTop) layout.messageGap.dp else layout.groupGap.dp,
                            com.promtuz.chat.ui.stage.ChatMotion.spec(), label = "message group gap",
                        )
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
                                    // the context menu re-draws this row lifted; hide the original
                                    .graphicsLayer { alpha = if (menu.anchor?.msg?.key == chatRow.msg.key) 1f - menu.lift else 1f },
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
                                    onMediaTap = { did ->
                                        val (items, index) = chatMediaItems(context, messages, name, did) { confirmDelete = it }
                                        MediaViewer.open(items, index)
                                    },
                                    onTap = (chatRow.msg.content as? MessageContent.Sticker)?.let { s ->
                                        { packSheet = s.sticker }
                                    },
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
                    is ChatRow.System -> when (val content = chatRow.msg.content) {
                        is MessageContent.Call -> CallRow(content, Modifier.padding(top = layout.groupGap.dp))
                        is MessageContent.System -> SystemRow(content, Modifier.padding(top = layout.groupGap.dp))
                        else -> {}
                    }
                    is ChatRow.Date -> ChatDateDivider(chatRow.date, calendar.today) { selectedDate = chatRow.date }
                    is ChatRow.Typing -> {
                        val gap by androidx.compose.animation.core.animateDpAsState(
                            if (chatRow.mergedTop) layout.messageGap.dp else layout.groupGap.dp,
                            com.promtuz.chat.ui.stage.ChatMotion.spec(), label = "typing group gap",
                        )
                        TypingBubble(Modifier.padding(top = gap), mergedTop = chatRow.mergedTop)
                    }
                }
            }

            }
            // Drawn after the stage so it fades the messages, not the wallpaper
            // behind them, and spans exactly the bar's own live footprint — the
            // composer's top edge to the bottom of the screen — so it tracks the
            // composer growing rather than lagging it.
            Box(
                Modifier
                    .align(Alignment.BottomCenter)
                    .fillMaxWidth()
                    .height(with(density) { metrics.bottomPx.toDp() })
                    .background(bottomScrim()),
            )
        }
        }

        menu.anchor?.let { anchor ->
            // Follow the actual row, including viewport limits when editing a
            // multiline message grows the field as well as the action area.
            MessageContextMenu(
                state = menu,
                quickReactions = QuickReactions,
                anchorOffsetY = { stage.pinnedOffsetY },
                actionGroups = menuActionsFor(anchor.msg, viewModel, onDelete = { confirmDelete = it }) { menu.close() },
                onReact = { viewModel.toggleReaction(anchor.msg, it); menu.close() },
            )
        }

        packSheet?.let { ref ->
            StickerPackSheet(
                ref = ref,
                viewModel = stickersVM,
                onDismiss = { packSheet = null },
                onAddImages = { pack -> packSheet = null; appVM.navigator.push(Routes.NewStickerPack(pack)) },
            )
        }

        selectedDate?.let { date ->
            com.promtuz.chat.ui.components.ChatDatePicker(
                initialDate = date,
                today = calendar.today,
                onDismiss = { selectedDate = null },
                onJump = { viewModel.jumpToDate(it, calendar.zone) },
            )
        }

        confirmDelete?.let { msg ->
            DeleteConfirmDialog(
                msg = msg,
                onConfirm = {
                    msg.dispatchIdHex?.let { viewModel.delete(it, forEveryone = msg.outgoing); MediaViewer.remove(it) }
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
        if (actionable) add(MenuAction("Reply", R.drawable.oi_reply) {
            viewModel.beginReply(msg); close()
        })
        if (actionable) add(MenuAction("Forward", R.drawable.oi_forward) { close() })
        // Only prose copies: a voice note or a sticker has no text to put on the clipboard.
        if (!msg.deleted && msg.content !is MessageContent.Voice && msg.content !is MessageContent.Sticker)
            add(MenuAction("Copy", R.drawable.oi_copy) {
                val text = (msg.content as? MessageContent.Text)?.text.orEmpty()
                scope.launch {
                    clipboard.setClipEntry(ClipEntry(ClipData.newPlainText("message", text)))
                }
                close()
            })
        // A voice note or a sticker has no text to edit and nothing to swap in for it.
        if (actionable && msg.outgoing && msg.content !is MessageContent.Voice && msg.content !is MessageContent.Sticker)
            add(MenuAction("Edit", R.drawable.oi_edit) {
            viewModel.beginEdit(msg); close()
        })
    }
    val destructive = buildList {
        if (msg.dispatchIdHex != null) add(MenuAction("Delete", R.drawable.oi_trash, destructive = true) {
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
 * A finished call, centred like a system line but with a phone icon and, when
 * it connected, its length.
 */
@Composable
private fun CallRow(content: MessageContent.Call, modifier: Modifier = Modifier) {
    val marker = LocalChatColors.current.marker
    val label = when {
        content.durationSecs != null -> {
            val s = content.durationSecs
            val length = if (s >= 60) "${s / 60} min ${s % 60} s" else "$s s"
            "${if (content.outgoing) "Outgoing" else "Incoming"} call · $length"
        }
        content.missed -> "Missed call"
        content.outgoing -> "No answer"
        else -> "Call declined"
    }
    Row(
        modifier.fillMaxWidth().padding(horizontal = 32.dp, vertical = 3.dp),
        horizontalArrangement = Arrangement.Center,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        com.promtuz.chat.ui.components.DrawableIcon(
            com.promtuz.chat.R.drawable.i_phone,
            Modifier.size(18.dp).padding(end = 6.dp),
            tint = marker.copy(alpha = 0.6f),
        )
        Text(
            label,
            style = MaterialTheme.typography.labelMedium,
            color = marker.copy(alpha = 0.6f),
            textAlign = TextAlign.Center,
        )
    }
}

/**
 * A membership or title change: centred, quiet, and deliberately not a bubble —
 * nobody said it *to* anyone, so it carries no author, no tail and no actions.
 */
@Composable
private fun SystemRow(content: MessageContent.System, modifier: Modifier = Modifier) {
    val marker = LocalChatColors.current.marker
    Row(
        modifier.fillMaxWidth().padding(horizontal = 32.dp, vertical = 3.dp),
        horizontalArrangement = Arrangement.Center,
    ) {
        Text(
            BubbleTextLayouts.systemLine(content),
            style = MaterialTheme.typography.labelMedium,
            color = marker.copy(alpha = 0.6f),
            textAlign = TextAlign.Center,
        )
    }
}

private fun rowKey(row: ChatRow): Any = when (row) {
    is ChatRow.Msg -> row.msg.key
    is ChatRow.System -> row.msg.key
    is ChatRow.Typing -> "typing"
    is ChatRow.Date -> row
}

/**
 * The stage's insets, with the bottom resolved on each call rather than captured.
 * [MessageStage] asks during its measure pass, so the composer's live footprint
 * reaches the walk as a layout read — the messages track the bar frame for frame
 * without a recomposition between them.
 */
private class ComposerPadding(
    private val top: Dp,
    private val metrics: ComposerMetrics,
    private val density: Density,
) : PaddingValues {
    override fun calculateTopPadding(): Dp = top
    override fun calculateBottomPadding(): Dp = with(density) { metrics.bottomPx.toDp() }
    override fun calculateLeftPadding(layoutDirection: LayoutDirection): Dp = 0.dp
    override fun calculateRightPadding(layoutDirection: LayoutDirection): Dp = 0.dp
}
