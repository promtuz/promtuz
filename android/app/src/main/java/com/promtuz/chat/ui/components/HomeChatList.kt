package com.promtuz.chat.ui.components

import androidx.compose.foundation.clickable
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.ui.draw.clip
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.sp
import com.promtuz.chat.R
import com.promtuz.chat.ui.theme.Promtuz
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.lifecycle.repeatOnLifecycle
import androidx.compose.runtime.Composable
import androidx.compose.runtime.SideEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.domain.model.Activity
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM

internal const val HomeSearchItemKey = "home-search"

@Composable
fun HomeChatList(innerPadding: PaddingValues, appViewModel: AppVM, menuState: HomeMenuState,
    listState: LazyListState,
) {
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
    val all by appViewModel.chats.collectAsState()
    val chats = androidx.compose.runtime.remember(all) { all.filterNot { it.request } }
    val requests = androidx.compose.runtime.remember(all) { all.filter { it.request } }
    val pins = remember(chats) { chats.associate { it.conversationHex to it.pinned } }
    var previousPins by remember { mutableStateOf(pins) }
    SideEffect {
        if (pins.any { (id, pinned) -> previousPins[id]?.let { it != pinned } == true }) {
            // A pin reorders keyed rows; hold the viewport at its index instead of following
            // the old first row. Request it with the new data, before measurement.
            listState.requestScrollToItem(listState.firstVisibleItemIndex, listState.firstVisibleItemScrollOffset)
        }
        previousPins = pins
    }
    val presence by appViewModel.presenceByPeer.collectAsState()
    val activity by appViewModel.activityByChat.collectAsState()

    LazyColumn(
        state = listState,
        modifier = Modifier.fillMaxSize(),
        contentPadding = innerPadding.listPadding(0.dp, 0.dp, 24.dp),
    ) {

        item(key = HomeSearchItemKey) {
            HomeSearchBar { appViewModel.navigator.push(Routes.Search) }
        }
        if (chats.isEmpty() && requests.isEmpty()) item(key = "empty") {
            HomeEmpty(Modifier.fillParentMaxHeight(0.65f))
        }

        if (requests.isNotEmpty()) item(key = "message-requests") {
            MessageRequestsRow(requesters(requests), requests.size) {
                appViewModel.navigator.push(Routes.MessageRequests)
            }
        }
        itemsIndexed(chats, key = { _, c -> c.conversationHex }) { _, chat ->
            // Presence is per person, so a group shows none; typing is per chat.
            HomeChatListItem(
                chat = chat,
                presence = chat.peerHex?.let { presence[it] },
                typing = Activity.Typing in
                    Activity.fromBits(activity[chat.conversationHex] ?: 0),
                pinned = chat.pinned,
                muted = chat.muted && !chat.locked,
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
private fun HomeEmpty(modifier: Modifier) {
    Box(
        modifier.padding(32.dp),
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

@Composable
private fun MessageRequestsRow(names: String, count: Int, onClick: () -> Unit) {
    val c = Promtuz.colors
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onClick).padding(horizontal = 16.dp, vertical = 9.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(52.dp).clip(SquircleShape).background(c.cardHigh), contentAlignment = Alignment.Center) {
            DrawableIcon(R.drawable.i_user_add, size = 24.dp, tint = c.accent)
        }
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(3.dp)) {
            Text("Message requests", style = TextStyle(fontSize = 16.sp, lineHeight = 22.sp, fontWeight = FontWeight.Medium), color = c.text)
            Text(names, style = TextStyle(fontSize = 14.5.sp, lineHeight = 20.sp), color = c.muted, maxLines = 1, overflow = TextOverflow.Ellipsis)
        }
        Box(
            Modifier.defaultMinSize(22.dp, 22.dp).clip(RoundedCornerShape(8.dp)).background(c.switchOff).padding(horizontal = 7.dp),
            contentAlignment = Alignment.Center,
        ) { Text("$count", style = TextStyle(fontSize = 12.sp, lineHeight = 16.sp, fontWeight = FontWeight.Medium), color = c.text) }
    }
}
