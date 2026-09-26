package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.*
import androidx.compose.foundation.gestures.ScrollableState
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.nestedscroll.nestedScroll

/** Both endpoints are opaque: transparent black must never enter the color tween. */
@Composable
fun appTopBarColors(): TopAppBarColors = TopAppBarDefaults.topAppBarColors(
    containerColor = MaterialTheme.colorScheme.background,
    scrolledContainerColor = MaterialTheme.colorScheme.surfaceContainerLow,
)

/**
 * Standard screen chrome. Search/selection headers can own their title content.
 * Home keeps its gradient scrim and Chat keeps its wallpaper blur; neither uses
 * the opaque scrolling surface provided here. Camera overlays supply their own colors.
 */
@Composable
fun AppTopBar(
    title: @Composable () -> Unit,
    modifier: Modifier = Modifier,
    navigationIcon: @Composable () -> Unit = { GoBackButton() },
    actions: @Composable RowScope.() -> Unit = {},
    scrollBehavior: TopAppBarScrollBehavior? = null,
    windowInsets: WindowInsets = TopAppBarDefaults.windowInsets,
    colors: TopAppBarColors = appTopBarColors(),
    connectionStatus: Boolean = true,
) {
    TopAppBar(
        title = {
            Box(Modifier.centerBarTitle(screenTitleStyle())) {
                if (connectionStatus) ConnectionAwareTitle(title) else ProvideTextStyle(screenTitleStyle(), title)
            }
        },
        modifier = modifier,
        navigationIcon = navigationIcon,
        actions = actions,
        scrollBehavior = scrollBehavior,
        windowInsets = windowInsets,
        colors = colors,
    )
}

/** One owner for toolbar scrolling and system insets, including custom screen headers. */
@Composable
fun ScreenScaffold(
    topBar: @Composable (TopAppBarScrollBehavior) -> Unit,
    modifier: Modifier = Modifier,
    scrollableState: ScrollableState? = null,
    snackbarHost: @Composable () -> Unit = {},
    floatingActionButton: @Composable () -> Unit = {},
    floatingActionButtonPosition: FabPosition = FabPosition.End,
    content: @Composable (PaddingValues) -> Unit,
) {
    val scrollBehavior = if (scrollableState == null) TopAppBarDefaults.pinnedScrollBehavior()
        else TopAppBarDefaults.pinnedScrollBehavior(scrollableState = scrollableState)
    Scaffold(
        modifier = modifier.fillMaxSize().nestedScroll(scrollBehavior.nestedScrollConnection),
        topBar = { topBar(scrollBehavior) },
        snackbarHost = snackbarHost,
        floatingActionButton = floatingActionButton,
        floatingActionButtonPosition = floatingActionButtonPosition,
        content = content,
    )
}
