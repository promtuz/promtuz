package com.promtuz.chat.ui.components

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.lifecycle.repeatOnLifecycle
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.domain.model.Activity
import com.promtuz.chat.presentation.viewmodel.AppVM

@Composable
fun HomeChatList(innerPadding: PaddingValues, appViewModel: AppVM, menuState: HomeMenuState) {
    val listState = androidx.compose.foundation.lazy.rememberLazyListState()
    val lifecycle = androidx.lifecycle.compose.LocalLifecycleOwner.current.lifecycle
    val foreground = com.promtuz.chat.navigation.LocalNavForeground.current
    val arrivals by appViewModel.unseenOnHome.collectAsState()
    androidx.compose.runtime.LaunchedEffect(listState, lifecycle, arrivals, foreground) {
        if (!foreground) return@LaunchedEffect
        lifecycle.repeatOnLifecycle(androidx.lifecycle.Lifecycle.State.RESUMED) {
            androidx.compose.runtime.snapshotFlow {
                listState.layoutInfo.visibleItemsInfo.mapNotNull { it.key as? String }.toSet()
            }.collect { appViewModel.sawHomeRows(it) }
        }
    }
    val direction = LocalLayoutDirection.current
    val all by appViewModel.chats.collectAsState()
    val chats = androidx.compose.runtime.remember(all) { all.filterNot { it.request } }
    val requests = androidx.compose.runtime.remember(all) { all.filter { it.request } }
    val presence by appViewModel.presenceByPeer.collectAsState()
    val activity by appViewModel.activityByChat.collectAsState()

    if (chats.isEmpty() && requests.isEmpty()) {
        HomeEmpty(innerPadding)
        return
    }

    LazyColumn(
        state = listState,
        modifier = Modifier.padding(
            start = innerPadding.calculateLeftPadding(direction),
            end = innerPadding.calculateRightPadding(direction),
        ).fillMaxSize(),
        contentPadding = PaddingValues(
            top = innerPadding.calculateTopPadding(),
            bottom = innerPadding.calculateBottomPadding() + 24.dp,
        ),
    ) {

        if (requests.isNotEmpty()) item(key = "message-requests") {
            androidx.compose.material3.ListItem(
                modifier = Modifier.clickable { appViewModel.navigator.push(com.promtuz.chat.navigation.Routes.MessageRequests) },
                headlineContent = { Text("Message requests") },
                supportingContent = { Text(requesters(requests), maxLines = 1, overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis) },
                leadingContent = { DrawableIcon(com.promtuz.chat.R.drawable.i_user_add, size = 28.dp) },
            )
        }
        itemsIndexed(chats, key = { _, c -> c.conversationHex }) { _, chat ->
            // Presence is per-person, so a group — which has no single
            // counterpart — shows none. Typing is per-chat, so a group has it.
            HomeChatListItem(
                chat = chat,
                presence = chat.peerHex?.let { presence[it] },
                typing = Activity.Typing in
                    Activity.fromBits(activity[chat.conversationHex] ?: 0),
                pinned = chat.pinned,
                muted = chat.muted,
                menuState = menuState,
                onOpen = { appViewModel.openChat(chat.conversationHex, chat.name) },
                onPin = { ChatPrefs.togglePin(chat.conversationHex, !chat.pinned) },
                onMute = { ChatPrefs.toggleMute(chat.conversationHex, !chat.muted) },
                onMarkRead = { appViewModel.markConversationRead(chat.conversationHex) },
                onClearHistory = { appViewModel.clearHistory(chat.conversationHex) },
                onDelete = { appViewModel.deleteChat(chat) },
                onLeaveAndDelete = { appViewModel.leaveAndDelete(chat) },
                modifier = Modifier.animateItem(),
            )
        }

    }
}

private fun requesters(requests: List<com.promtuz.chat.domain.model.ChatSummary>): String {
    val first = requests.first().name
    return when (requests.size) {
        1 -> first
        2 -> "$first and ${requests[1].name}"
        else -> "$first and ${requests.size - 1} others"
    }
}

@Composable
private fun HomeEmpty(innerPadding: PaddingValues) {
    Box(
        Modifier
            .fillMaxSize()
            .padding(innerPadding)
            .padding(32.dp),
        contentAlignment = Alignment.Center,
    ) {
        Column(
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Text(
                "No chats yet",
                style = MaterialTheme.typography.titleMediumEmphasized,
                color = MaterialTheme.colorScheme.onSurface,
            )
            Text(
                "Add a contact to start messaging.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                textAlign = TextAlign.Center,
            )
        }
    }
}
