package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.calculateEndPadding
import androidx.compose.foundation.layout.calculateStartPadding
import androidx.compose.foundation.layout.RowScope
import androidx.compose.material3.FabPosition
import androidx.compose.material3.TopAppBarColors
import androidx.compose.foundation.gestures.ScrollableState
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

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

/** The screen's padding plus [h] on each side and [top] and [bottom] more, for its scrolling content. */
@Composable
fun PaddingValues.listPadding(h: Dp = 18.dp, top: Dp = 12.dp, bottom: Dp = 24.dp): PaddingValues {
    val direction = LocalLayoutDirection.current
    return PaddingValues(
        start = calculateStartPadding(direction) + h,
        top = calculateTopPadding() + top,
        end = calculateEndPadding(direction) + h,
        bottom = calculateBottomPadding() + bottom,
    )
}
