package com.promtuz.chat.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.ScrollableState
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.remember
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.util.freezeOnExit
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.HazeStyle
import dev.chrisbanes.haze.HazeTint
import dev.chrisbanes.haze.hazeEffect
import dev.chrisbanes.haze.hazeSource
import dev.chrisbanes.haze.rememberHazeState

private class ScreenGlass(val haze: HazeState, val page: Color)

/** Set by [ScreenScaffold]; a bar outside one has nothing to blur and stays opaque. */
private val LocalScreenGlass = staticCompositionLocalOf<ScreenGlass?> { null }

/** Both endpoints are opaque: transparent black must never enter the color tween. */
@Composable
fun appTopBarColors(): TopAppBarColors = TopAppBarDefaults.topAppBarColors(
    containerColor = MaterialTheme.colorScheme.background,
    scrolledContainerColor = MaterialTheme.colorScheme.surfaceContainerLow,
)

@Composable
fun glassStyle(page: Color = Promtuz.colors.page): HazeStyle {
    val card = Promtuz.colors.card
    return remember(page, card) { HazeStyle(page, HazeTint(card.copy(alpha = 0.72f)), 32.dp, 0f) }
}

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
    // At rest the bar is the page; glass fades in once content slides under it.
    val glass = LocalScreenGlass.current
    val glassModifier = if (glass == null) Modifier else Modifier
        .freezeOnExit()
        .hazeEffect(glass.haze, glassStyle(glass.page)) { alpha = scrollBehavior?.state?.overlappedFraction ?: 0f }
    TopAppBar(
        title = {
            Box(Modifier.centerBarTitle(screenTitleStyle())) {
                if (connectionStatus) ConnectionAwareTitle(title) else ProvideTextStyle(screenTitleStyle(), title)
            }
        },
        modifier = modifier.then(glassModifier),
        navigationIcon = navigationIcon,
        actions = actions,
        expandedHeight = 60.dp,
        scrollBehavior = scrollBehavior,
        windowInsets = windowInsets,
        colors = if (glass == null) colors
            else TopAppBarDefaults.topAppBarColors(containerColor = Color.Transparent, scrolledContainerColor = Color.Transparent),
    )
}

/**
 * One owner for toolbar scrolling and system insets, including custom screen headers. Content
 * draws under the bar (apply the padding as contentPadding) so the bar's glass has something to blur.
 */
@Composable
fun ScreenScaffold(
    topBar: @Composable (TopAppBarScrollBehavior) -> Unit,
    modifier: Modifier = Modifier,
    scrollableState: ScrollableState? = null,
    containerColor: Color = MaterialTheme.colorScheme.background,
    snackbarHost: @Composable () -> Unit = {},
    floatingActionButton: @Composable () -> Unit = {},
    floatingActionButtonPosition: FabPosition = FabPosition.End,
    content: @Composable (PaddingValues) -> Unit,
) {
    val scrollBehavior = if (scrollableState == null) TopAppBarDefaults.pinnedScrollBehavior()
        else TopAppBarDefaults.pinnedScrollBehavior(scrollableState = scrollableState)
    val haze = rememberHazeState()
    val glass = remember(haze, containerColor) { ScreenGlass(haze, containerColor) }
    CompositionLocalProvider(LocalScreenGlass provides glass) {
        Scaffold(
            modifier = modifier.fillMaxSize().nestedScroll(scrollBehavior.nestedScrollConnection),
            topBar = { topBar(scrollBehavior) },
            snackbarHost = snackbarHost,
            floatingActionButton = floatingActionButton,
            floatingActionButtonPosition = floatingActionButtonPosition,
            containerColor = containerColor,
        ) { padding ->
            Box(Modifier.fillMaxSize().background(containerColor).hazeSource(haze)) { content(padding) }
        }
    }
}
