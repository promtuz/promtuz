package com.promtuz.chat.ui.screens

import androidx.appcompat.content.res.AppCompatResources
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListPrefetchStrategy
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.*
import androidx.compose.ui.graphics.*
import androidx.compose.ui.platform.*
import androidx.compose.ui.unit.*
import androidx.core.graphics.drawable.toBitmap
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.Chat
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.ui.components.ChatBottomBar
import com.promtuz.chat.ui.components.ChatTopBar
import com.promtuz.chat.ui.components.MessageBubble
import dev.chrisbanes.haze.hazeSource
import dev.chrisbanes.haze.rememberHazeState

/**
 * TODO:
 *  - Make almost everything customizable
 *  - Make a system for easy value storage and retrieval from preferences i suppose,
 *  - Make structures for UI schemes and styles like the order of elements, colors, spacing, roundness.
 *  - Make screen to grant users option to choose whatever combination of customization they like
 *  - Make exporting and importing all style preferences possible
 */
@OptIn(ExperimentalLayoutApi::class, ExperimentalFoundationApi::class)
@Composable
fun ChatScreen(
    chat: Chat,
    viewModel: ChatVM,
) {
    val direction = LocalLayoutDirection.current
    val colors = MaterialTheme.colorScheme
    val hazeState = rememberHazeState()
    val lazyState = rememberLazyListState(
        prefetchStrategy = LazyListPrefetchStrategy(nestedPrefetchItemCount = 8)
    )
    val interactionSource = remember { MutableInteractionSource() }

    val messages by viewModel.messages.collectAsState()

    val ctx = LocalContext.current
    val drawablePattern = AppCompatResources.getDrawable(ctx, R.drawable.pattern_chat_topography)!!
    val bgPattern = drawablePattern.toBitmap(1200, 1200)
        .asImageBitmap()
    val shader = ImageShader(bgPattern, TileMode.Repeated, TileMode.Repeated)
    val brush = ShaderBrush(shader)

    Scaffold(
        Modifier
            .fillMaxSize()
            .imePadding(),
        topBar = { ChatTopBar(chat, viewModel, hazeState) },
        bottomBar = { ChatBottomBar(hazeState, viewModel, interactionSource) }
    ) { padding ->
        LazyColumn(
            Modifier
                .fillMaxSize()
                .hazeSource(hazeState)
                .background(colors.surface)
                .background(brush, alpha = 0.1f)
                .padding(
                    start = padding.calculateLeftPadding(direction),
                    end = padding.calculateRightPadding(direction),
                    top = 0.dp,
                    bottom = 0.dp
                ),
            contentPadding = PaddingValues(
                top = padding.calculateTopPadding() + 6.dp,
                bottom = padding.calculateBottomPadding() + 6.dp,
            ),
            state = lazyState,
            reverseLayout = true,
            verticalArrangement = Arrangement.spacedBy(2.dp, Alignment.Bottom)
        ) {
            item { Spacer(Modifier.height(3.dp)) }
            items(messages, key = { it.id }) {
                MessageBubble(it)
            }
        }
    }
}