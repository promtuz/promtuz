package com.promtuz.chat.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Scaffold
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.ui.components.ChatBottomBar
import com.promtuz.chat.ui.components.ChatTopBar
import com.promtuz.chat.ui.components.MessageBubble

@Composable
fun ChatScreen(name: String, viewModel: ChatVM) {
    val messages by viewModel.messages.collectAsState()
    Scaffold(
        topBar = { ChatTopBar(name, viewModel) },
        bottomBar = { ChatBottomBar(viewModel) },
    ) { padding ->
        LazyColumn(
            Modifier.fillMaxSize().padding(padding),
            reverseLayout = true,
            verticalArrangement = Arrangement.spacedBy(2.dp),
        ) {
            items(messages, key = { it.key }) { MessageBubble(it) }
        }
    }
}
