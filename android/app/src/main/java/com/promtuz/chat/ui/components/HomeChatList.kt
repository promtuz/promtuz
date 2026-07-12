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
    val activity by appViewModel.activityByPeer.collectAsState()
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

        itemsIndexed(chats, key = { _, c -> c.peerHex }) { _, chat ->
            HomeChatListItem(
                chat = chat,
                presence = presence[chat.peerHex],
                typing = Activity.Typing in Activity.fromBits(activity[chat.peerHex] ?: 0),
                pinned = chat.peerHex in pinned,
                muted = chat.peerHex in muted,
                menuState = menuState,
                onOpen = { appViewModel.openChat(chat.peerHex, chat.name) },
                onPin = { ChatPrefs.togglePin(chat.peerHex) },
                onMute = { ChatPrefs.toggleMute(chat.peerHex) },
                onMarkRead = { appViewModel.markConversationRead(chat.peerHex) },
                onDelete = { appViewModel.deleteChat(chat.peerHex) },
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
