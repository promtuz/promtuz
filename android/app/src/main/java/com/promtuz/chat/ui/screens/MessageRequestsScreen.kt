package com.promtuz.chat.ui.screens

import android.widget.Toast
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.LifecycleResumeEffect
import com.promtuz.chat.domain.model.Activity
import com.promtuz.chat.navigation.LocalNavForeground
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.components.HomeChatListItem
import com.promtuz.chat.ui.components.HomeContextMenu
import com.promtuz.chat.ui.components.HomeMenuState
import com.promtuz.chat.ui.components.RequestDecision
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.core.CoreBridge
import com.promtuz.core.push.PushNotifier
import kotlinx.coroutines.launch
import org.koin.compose.koinInject

@Composable
fun MessageRequestsScreen() {
    if (LocalNavForeground.current) LifecycleResumeEffect(Unit) {
        PushNotifier.viewingRequests(true)
        onPauseOrDispose { PushNotifier.viewingRequests(false) }
    }
    val app = koinInject<AppVM>()
    val all by app.chats.collectAsState()
    val requests = remember(all) { all.filter { it.request } }
    val activity by app.activityByChat.collectAsState()
    val menuState = remember { HomeMenuState() }
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    Box {
        SimpleScreen({ Text("Message requests") }) { padding ->
            LazyColumn(Modifier.fillMaxSize(), contentPadding = padding) {
                if (requests.isEmpty()) item {
                    Text("No message requests", Modifier.padding(24.dp), color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
                items(requests, key = { it.conversationHex }) { chat ->
                    HomeChatListItem(
                        chat = chat,
                        presence = null,
                        typing = Activity.Typing in Activity.fromBits(activity[chat.conversationHex] ?: 0),
                        pinned = false,
                        muted = chat.muted,
                        menuState = menuState,
                        onOpen = { app.openChat(chat.conversationHex, chat.name) },
                        onPin = {},
                        onMute = {},
                        onMarkRead = {},
                        onClearHistory = {},
                        onDelete = {},
                        onLeaveAndDelete = {},
                        modifier = Modifier.animateItem(),
                        onRequestDecision = { decision ->
                            val peer = chat.peerHex?.fromHex() ?: return@HomeChatListItem
                            scope.launch {
                                runCatching {
                                    if (decision == RequestDecision.Block) CoreBridge.blockMessageRequest(peer)
                                    else CoreBridge.deleteMessageRequest(peer)
                                }.onFailure {
                                    Toast.makeText(context, "Couldn't update the request", Toast.LENGTH_SHORT).show()
                                }
                            }
                        },
                    )
                }
            }
        }
        HomeContextMenu(menuState)
    }
}
