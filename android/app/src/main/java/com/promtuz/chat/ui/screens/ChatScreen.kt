package com.promtuz.chat.ui.screens

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.Scaffold
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.ui.appearance.LocalChatAppearance
import com.promtuz.chat.ui.components.ChatBottomBar
import com.promtuz.chat.ui.components.ChatTopBar
import com.promtuz.chat.ui.components.MessageBubble
import com.promtuz.chat.ui.components.rememberChatWallpaper
import kotlin.math.abs

@Composable
fun ChatScreen(name: String, viewModel: ChatVM) {
    val messages by viewModel.messages.collectAsState()
    val appearance = LocalChatAppearance.current
    val layout = appearance.layout
    val mergeWindowMs = layout.mergeWindowSecs * 1000L
    val wallpaper = rememberChatWallpaper(appearance.wallpaper)

    Scaffold(
        topBar = { ChatTopBar(name, viewModel) },
        bottomBar = { ChatBottomBar(viewModel) },
    ) { padding ->
        Box(Modifier.fillMaxSize().then(wallpaper)) {
            LazyColumn(
                Modifier.fillMaxSize().padding(padding),
                reverseLayout = true,
            ) {
                // Newest at index 0, drawn at the bottom (reverseLayout). "Above" a bubble is the
                // older neighbour (i+1); "below" is the newer (i-1). Same author within the merge
                // window groups; the gap above each bubble is small inside a group, larger between.
                itemsIndexed(messages, key = { _, m -> m.key }) { i, m ->
                    val older = messages.getOrNull(i + 1)
                    val newer = messages.getOrNull(i - 1)
                    val mergedTop = older != null && sameGroup(m, older, mergeWindowMs)
                    val mergedBottom = newer != null && sameGroup(m, newer, mergeWindowMs)
                    val gapAbove = if (mergedTop) layout.messageGap.dp else layout.groupGap.dp
                    MessageBubble(m, mergedTop, mergedBottom, Modifier.padding(top = gapAbove))
                }
            }
        }
    }
}

private fun sameGroup(a: UiMessage, b: UiMessage, windowMs: Long): Boolean =
    a.outgoing == b.outgoing && abs(a.timestampMs - b.timestampMs) <= windowMs
