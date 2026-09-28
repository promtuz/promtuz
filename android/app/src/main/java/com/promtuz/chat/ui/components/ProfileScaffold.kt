package com.promtuz.chat.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.SideEffect
import androidx.compose.ui.BiasAlignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.lerp
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.lerp

/** One photo and name move from the expanded profile into the standard Back/overflow toolbar. */
@Composable
fun ProfileScaffold(
    name: String,
    photo: @Composable (Dp) -> Unit,
    actions: @Composable RowScope.() -> Unit = {},
    snackbarHost: @Composable () -> Unit = {},
    content: @Composable (PaddingValues) -> Unit,
) {
    val scroll = TopAppBarDefaults.exitUntilCollapsedScrollBehavior()
    val range = with(LocalDensity.current) { (196.dp - 64.dp).toPx() }
    SideEffect { scroll.state.heightOffsetLimit = -range }
    Scaffold(
        modifier = Modifier.fillMaxSize().nestedScroll(scroll.nestedScrollConnection),
        snackbarHost = snackbarHost,
        topBar = {
            val progress = scroll.state.collapsedFraction
            BoxWithConstraints(
                Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.background)
                    .windowInsetsPadding(TopAppBarDefaults.windowInsets)
                    .height(lerp(196.dp, 64.dp, progress)),
            ) {
                val photoSize = lerp(112.dp, 36.dp, progress)
                Box(Modifier.offset(x = lerp((maxWidth - photoSize) / 2, 56.dp, progress),
                    y = lerp(16.dp, 14.dp, progress))) { photo(photoSize) }
                Box(
                    Modifier.offset(x = lerp(16.dp, 104.dp, progress), y = lerp(144.dp, 14.dp, progress))
                        .width(lerp(maxWidth - 32.dp, (maxWidth - 160.dp).coerceAtLeast(0.dp), progress))
                        .height(36.dp),
                    contentAlignment = BiasAlignment(-progress, 0f),
                ) {
                    Text(name, style = lerp(MaterialTheme.typography.headlineSmall,
                        MaterialTheme.typography.titleLarge, progress), maxLines = 1, overflow = TextOverflow.Ellipsis)
                }
                // Keep the same controls and hit targets throughout the collapse.
                TopAppBar(
                    title = {}, navigationIcon = { GoBackButton() }, actions = actions,
                    windowInsets = WindowInsets(0),
                    colors = TopAppBarDefaults.topAppBarColors(containerColor = Color.Transparent),
                )
            }
        },
        content = content,
    )
}
