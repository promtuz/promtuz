package com.promtuz.chat.ui.screens

import dev.chrisbanes.haze.hazeSource
import dev.chrisbanes.haze.rememberHazeState
import androidx.compose.foundation.layout.*
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.zIndex
import com.promtuz.chat.navigation.LocalSearchMotion
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.clickable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.sp
import com.promtuz.chat.domain.model.MessageLocation
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.text.parseMessageDate
import com.promtuz.chat.utils.media.rememberAvatar
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.unit.dp
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.domain.model.ChatSummary
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.security.PrivateChatSession
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.withContext
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

/** Deliberately ephemeral: neither the query nor decrypted results are saveable state. */
@Composable
fun HomeSearchScreen(app: AppVM) {
    DisposableEffect(Unit) {
        val protection = com.promtuz.chat.security.PrivateChatSession.protectInput()
        onDispose { protection.close() }
    }
    var query by remember { mutableStateOf("") }
    var revealed by remember { mutableStateOf<ChatSummary?>(null) }
    var revealedId by remember { mutableStateOf<String?>(null) }
    val chats by app.chats.collectAsState()
    val session by PrivateChatSession.active.collectAsState()
    val focus = remember { FocusRequester() }
    val menu = remember { HomeMenuState() }
    val foreground = com.promtuz.chat.navigation.LocalNavForeground.current
    val currentQuery by rememberUpdatedState(query)
    val currentForeground by rememberUpdatedState(foreground)
    val unlockMutex = remember { Mutex() }
    LaunchedEffect(foreground) { if (foreground) focus.requestFocus() }
    LaunchedEffect(query, foreground) {
        if (!foreground) { query = ""; revealed = null; revealedId = null; return@LaunchedEffect }
        revealed = null
        revealedId = null
        PrivateChatSession.clear()
        app.clearPrivateSummary()
        if (query.isEmpty()) return@LaunchedEffect
        val expected = query
        delay(350)
        // UniFFI work cannot be interrupted midway. Complete it before checking whether this
        // query still owns its result, and revoke any late unlock before returning.
        withContext(NonCancellable) {
            unlockMutex.withLock {
                if (currentQuery != expected || !currentForeground || !PrivateChatSession.foreground) return@withLock
                val epoch = PrivateChatSession.generation
                val id = runCatching { CoreBridge.unlockPrivateConversation(expected, epoch) }.getOrNull()
                if (id != null) {
                    if (currentQuery == expected && currentForeground && PrivateChatSession.generation == epoch &&
                        PrivateChatSession.arm(id.toHex())) {
                        revealedId = id.toHex()
                        if (currentQuery != expected || !currentForeground || !PrivateChatSession.foreground || PrivateChatSession.generation != epoch) {
                            revealed = null
                            revealedId = null
                            PrivateChatSession.clear()
                            app.clearPrivateSummary()
                        }
                    } else { PrivateChatSession.clear(); app.clearPrivateSummary() }
                }
            }
        }
    }
    LaunchedEffect(revealedId, session, foreground) {
        val id = revealedId ?: return@LaunchedEffect
        if (!foreground || session != id) return@LaunchedEffect
        val expected = query
        val epoch = PrivateChatSession.generation
        observeQuery(setOf("messages", "incoming_receipts", "message_media", "contacts",
            "conversations", "conversation_members")) {
            runCatching { app.privateSearchResult(id.fromHex()) }.getOrNull()
        }.collect { summary ->
            if (currentQuery == expected && currentForeground && PrivateChatSession.foreground &&
                PrivateChatSession.active.value == id && PrivateChatSession.generation == epoch) {
                revealed = summary
            }
        }
    }
    DisposableEffect(Unit) {
        onDispose {
            // Opening the chat retains its session; leaving search without opening it does not.
            val top = app.backStack.lastOrNull()
            if (top !is com.promtuz.chat.navigation.Routes.Chat) {
                PrivateChatSession.clear()
                app.clearPrivateSummary()
            }
        }
    }
    val ordinary = remember(chats, query) {
        if (query.isBlank()) emptyList() else chats.filter { it.name.contains(query, ignoreCase = true) }
    }
    val privateResult = revealed?.takeIf { it.conversationHex == session }
    val results = remember(privateResult, ordinary) {
        listOfNotNull(privateResult) + ordinary.filterNot { it.conversationHex == privateResult?.conversationHex }
    }
    var hits by remember { mutableStateOf<List<MessageHit>>(emptyList()) }
    LaunchedEffect(query, foreground) {
        hits = emptyList()
        if (!foreground || query.isBlank()) return@LaunchedEffect
        delay(250)
        val byId = chats.associateBy { it.conversationHex }
        val names = mutableMapOf<String, Map<String, String>>()
        hits = runCatching { CoreBridge.searchAllMessages(query) }.getOrDefault(emptyList()).mapNotNull { m ->
            val hex = m.conversationId.toHex()
            val chat = byId[hex]?.takeIf { !it.locked && !it.request } ?: return@mapNotNull null
            val sender = when {
                m.outgoing -> "You"
                !chat.isGroup -> null
                else -> names.getOrPut(hex) {
                    runCatching { CoreBridge.members(m.conversationId) }.getOrDefault(emptyList())
                        .associate { it.ipk.toHex() to it.name }
                }[m.senderIpk?.toHex()]
            }
            MessageHit(chat, m.dispatchId.toHex(), m.content, m.timestamp.toLong() * 1000, sender)
        }
    }
    val motion = LocalSearchMotion.current
    val background = MaterialTheme.colorScheme.background
    val menuHaze = rememberHazeState()
    Box(Modifier.fillMaxSize().drawBehind {
        drawRect(background, alpha = motion.fraction().coerceIn(0f, 1f))
    }) {
        Column(Modifier.fillMaxSize().windowInsetsPadding(WindowInsets.statusBars.only(WindowInsetsSides.Top))) {
            Box(Modifier.zIndex(1f)) {
                HomeSearchBar(expanded = true, query = query,
                    onQueryChange = { query = it; revealed = null; revealedId = null }, focusRequester = focus)
            }
            LazyColumn(Modifier.fillMaxWidth().weight(1f).hazeSource(menuHaze).graphicsLayer {
                alpha = motion.fraction().coerceIn(0f, 1f)
            }, contentPadding = WindowInsets.navigationBars.asPaddingValues().listPadding(12.dp, 0.dp, 24.dp)) {
                if (results.isEmpty() && hits.isEmpty() && query.isNotBlank()) item {
                    Box(Modifier.fillMaxWidth().padding(24.dp), contentAlignment = Alignment.Center) {
                        Text("No results", style = RowSubtitleStyle, color = Promtuz.colors.muted)
                    }
                }
                if (results.isNotEmpty()) {
                    item("chats") { CardSectionLabel("Chats", Modifier.lazyCardRow(0, results.size + 1)) }
                    itemsIndexed(results, key = { _, it -> it.conversationHex }) { i, chat ->
                        HomeChatListItem(chat, presence = null, typing = false, pinned = chat.pinned,
                            muted = chat.muted && !chat.locked, menuState = menu, privateChat = chat.conversationHex == privateResult?.conversationHex,
                            modifier = Modifier.lazyCardRow(i + 1, results.size + 1, dividerStart = 76.dp, divider = i > 0),
                            onOpen = {
                                if (chat.conversationHex != privateResult?.conversationHex) {
                                    PrivateChatSession.clear()
                                    app.clearPrivateSummary()
                                }
                                app.openChat(chat.conversationHex, chat.name)
                            },
                            onPin = { ChatPrefs.togglePin(chat.conversationHex, !chat.pinned) },
                            onMute = { ChatPrefs.toggleMute(chat.conversationHex, !chat.muted) },
                            onMarkRead = { app.markConversationRead(chat.conversationHex) },
                            onClearHistory = { app.clearHistory(chat.conversationHex) },
                            onDelete = { app.deleteChat(chat) }, onLeaveAndDelete = { app.leaveAndDelete(chat) })
                    }
                }
                if (hits.isNotEmpty()) {
                    if (results.isNotEmpty()) item("gap") { Spacer(Modifier.height(12.dp)) }
                    item("messages") { CardSectionLabel("Messages", Modifier.lazyCardRow(0, hits.size + 1)) }
                    itemsIndexed(hits, key = { _, it -> "m" + it.dispatchHex }) { i, hit ->
                        MessageHitRow(hit, query, Modifier.lazyCardRow(i + 1, hits.size + 1, dividerStart = 76.dp, divider = i > 0)) {
                            PrivateChatSession.clear()
                            app.clearPrivateSummary()
                            app.showMessage(MessageLocation(hit.chat.conversationHex, hit.chat.name, hit.dispatchHex, hit.timestampMs))
                        }
                    }
                }
            }
        }
        HomeContextMenu(menu, menuHaze)
    }
}

private class MessageHit(val chat: ChatSummary, val dispatchHex: String, val text: String, val timestampMs: Long, val sender: String?)

@Composable
private fun CardSectionLabel(text: String, modifier: Modifier) {
    Text(text, modifier.fillMaxWidth().padding(start = 16.dp, end = 16.dp, top = 14.dp, bottom = 4.dp),
        style = TextStyle(fontSize = 14.sp, lineHeight = 20.sp, fontWeight = FontWeight.Medium), color = Promtuz.colors.accent)
}

@Composable
private fun MessageHitRow(hit: MessageHit, query: String, modifier: Modifier, onClick: () -> Unit) {
    val c = Promtuz.colors
    val chat = hit.chat
    val line = remember(hit, query) { highlight(hit.sender, hit.text, query.trim(), c.accent) }
    Row(
        modifier.fillMaxWidth().clickable(onClick = onClick).padding(horizontal = 16.dp, vertical = 10.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (chat.isGroup) GroupAvatar(title = chat.name, members = emptyList(), conversation = chat.conversationHex)
        else Avatar(chat.name, identityKey = chat.peerHex ?: chat.conversationHex, image = rememberAvatar(chat.peerHex))
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(3.dp)) {
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(chat.name, Modifier.weight(1f), style = HomeNameStyle, color = c.text, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(parseMessageDate(LocalContext.current, hit.timestampMs), style = HomeTimeStyle, color = c.muted)
            }
            Text(line, style = HomePreviewStyle, color = c.muted, maxLines = 1, overflow = TextOverflow.Ellipsis)
        }
    }
}

/** A match deep in a long message is pulled into view so the highlight stays on the one visible line. */
private fun highlight(sender: String?, text: String, query: String, accent: Color): AnnotatedString {
    var body = text.replace('\n', ' ')
    var at = body.indexOf(query, ignoreCase = true)
    if (at > 28) { body = "…" + body.substring(at - 12); at = body.indexOf(query, ignoreCase = true) }
    return buildAnnotatedString {
        if (sender != null) append("$sender: ")
        var from = 0
        while (at >= 0 && query.isNotEmpty()) {
            append(body.substring(from, at))
            withStyle(SpanStyle(color = accent, fontWeight = FontWeight.SemiBold)) { append(body.substring(at, at + query.length)) }
            from = at + query.length
            at = body.indexOf(query, from, ignoreCase = true)
        }
        append(body.substring(from))
    }
}
