package com.promtuz.chat.ui.screens

import androidx.compose.foundation.layout.*
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.zIndex
import com.promtuz.chat.navigation.LocalSearchMotion
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
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
        if (query.isBlank()) chats else chats.filter {
            it.name.contains(query, ignoreCase = true) || it.lastPreview.orEmpty().contains(query, ignoreCase = true)
        }
    }
    val privateResult = revealed?.takeIf { it.conversationHex == session }
    val results = remember(privateResult, ordinary) {
        listOfNotNull(privateResult) + ordinary.filterNot { it.conversationHex == privateResult?.conversationHex }
    }
    val motion = LocalSearchMotion.current
    val background = MaterialTheme.colorScheme.background
    Box(Modifier.fillMaxSize().drawBehind {
        drawRect(background, alpha = motion.fraction().coerceIn(0f, 1f))
    }) {
        Column(Modifier.fillMaxSize().windowInsetsPadding(WindowInsets.statusBars.only(WindowInsetsSides.Top))) {
            Box(Modifier.zIndex(1f)) {
                HomeSearchBar(expanded = true, query = query,
                    onQueryChange = { query = it; revealed = null; revealedId = null }, focusRequester = focus)
            }
            LazyColumn(Modifier.fillMaxWidth().weight(1f).graphicsLayer {
                alpha = motion.fraction().coerceIn(0f, 1f)
            }, contentPadding = WindowInsets.navigationBars.asPaddingValues().listPadding(0.dp, 0.dp, 24.dp)) {
                if (results.isEmpty() && query.isNotEmpty()) item {
                    Box(Modifier.fillMaxWidth().padding(24.dp), contentAlignment = Alignment.Center) {
                        Text("No results", color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                }
                items(results, key = { it.conversationHex }) { chat ->
                    HomeChatListItem(chat, presence = null, typing = false, pinned = chat.pinned,
                        muted = chat.muted && !chat.locked, menuState = menu, privateChat = chat.conversationHex == privateResult?.conversationHex,
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
        }
        HomeContextMenu(menu)
    }
}
