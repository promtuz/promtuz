package com.promtuz.chat.ui.components

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
    val direction = LocalLayoutDirection.current
    val chats by appViewModel.chats.collectAsState()
    val presence by appViewModel.presenceByPeer.collectAsState()
    val activity by appViewModel.activityByChat.collectAsState()
    val pinned by ChatPrefs.pinned.collectAsState()
    val muted by ChatPrefs.muted.collectAsState()

    if (chats.isEmpty()) {
        HomeEmpty(innerPadding)
        return
    }

    LazyColumn(
        Modifier.padding(
            start = innerPadding.calculateLeftPadding(direction),
            end = innerPadding.calculateRightPadding(direction),
        ),
    ) {
        item { Spacer(Modifier.height(innerPadding.calculateTopPadding())) }

        itemsIndexed(chats, key = { _, c -> c.conversationHex }) { _, chat ->
            // Presence is per-person, so a group — which has no single
            // counterpart — shows none. Typing is per-chat, so a group has it.
            HomeChatListItem(
                chat = chat,
                presence = chat.peerHex?.let { presence[it] },
                typing = Activity.Typing in
                    Activity.fromBits(activity[chat.conversationHex] ?: 0),
                pinned = chat.conversationHex in pinned,
                muted = chat.conversationHex in muted,
                menuState = menuState,
                onOpen = { appViewModel.openChat(chat.conversationHex, chat.name) },
                onPin = { ChatPrefs.togglePin(chat.conversationHex) },
                onMute = { ChatPrefs.toggleMute(chat.conversationHex) },
                onMarkRead = { appViewModel.markConversationRead(chat.conversationHex) },
                onDelete = { appViewModel.deleteChat(chat) },
                onLeaveAndDelete = { appViewModel.leaveAndDelete(chat) },
                modifier = Modifier.animateItem(),
            )
        }

        item { Spacer(Modifier.height(24.dp)) }
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
