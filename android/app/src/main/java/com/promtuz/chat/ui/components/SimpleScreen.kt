package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.RowScope
import androidx.compose.material3.FabPosition
import androidx.compose.material3.TopAppBarColors
import androidx.compose.foundation.gestures.ScrollableState
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier

@Composable
fun SimpleScreen(
    title: @Composable (() -> Unit),
    modifier: Modifier = Modifier,
    actions: @Composable RowScope.() -> Unit = {},
    topBarColors: TopAppBarColors = appTopBarColors(),
    topBarModifier: Modifier = Modifier,
    navigationIcon: @Composable () -> Unit = { GoBackButton() },
    connectionStatus: Boolean = true,
    scrollableState: ScrollableState? = null,
    snackbarHost: @Composable () -> Unit = {},
    floatingActionButton: @Composable () -> Unit = {},
    floatingActionButtonPosition: FabPosition = FabPosition.End,
    content: @Composable ((PaddingValues) -> Unit)
) {
    ScreenScaffold(
        modifier = modifier,
        topBar = { scrollBehavior ->
            AppTopBar(
                title = title,
                connectionStatus = connectionStatus,
                modifier = topBarModifier,
                navigationIcon = navigationIcon,
                colors = topBarColors,
                scrollBehavior = scrollBehavior,
                actions = actions
            )
        },
        scrollableState = scrollableState,
        snackbarHost = snackbarHost,
        floatingActionButton = floatingActionButton,
        floatingActionButtonPosition = floatingActionButtonPosition,
        content = content,
    )
}
