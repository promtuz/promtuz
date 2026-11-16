package com.promtuz.chat.ui.screens

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.ui.Modifier
import com.promtuz.chat.ui.theme.Promtuz
import dev.chrisbanes.haze.hazeSource
import dev.chrisbanes.haze.rememberHazeState
import androidx.compose.material3.Scaffold
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.runtime.getValue
import androidx.compose.runtime.derivedStateOf
import androidx.compose.foundation.lazy.rememberLazyListState
import com.promtuz.chat.ui.components.HomeSearchItemKey
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.components.HomeChatList
import com.promtuz.chat.ui.components.HomeContextMenu
import com.promtuz.chat.ui.components.HomeFab
import com.promtuz.chat.ui.components.HomeMenuState
import com.promtuz.chat.ui.components.HomeTopBar

@androidx.annotation.OptIn(androidx.camera.core.ExperimentalGetImage::class)
@Composable
fun HomeScreen(
    appViewModel: AppVM
) {
    // The overlay sits above the Scaffold so its scrim dims the bars and FAB too.
    val menuState = remember { HomeMenuState() }
    val listState = rememberLazyListState()
    val searchHidden by remember(listState) {
        derivedStateOf {
            val layout = listState.layoutInfo
            // LazyColumn can still report an item behind the overlaid home toolbar. Its end
            // must reach the unpadded viewport to count as a visible search affordance.
            layout.totalItemsCount > 0 && layout.visibleItemsInfo.none {
                it.key == HomeSearchItemKey && it.offset + it.size > 0
            }
        }
    }
    val haze = rememberHazeState()
    val menuHaze = rememberHazeState()
    val scrolled by animateFloatAsState(if (listState.canScrollBackward) 1f else 0f, label = "homeBarGlass")
    val page = Promtuz.colors.page
    Box {
        Box(Modifier.hazeSource(menuHaze)) { Scaffold(
            topBar = { HomeTopBar(appViewModel, showSearch = searchHidden, haze = haze, glass = { scrolled }) },
            floatingActionButton = { HomeFab(appViewModel) },
            containerColor = page,
        ) { innerPadding ->
            Box(Modifier.fillMaxSize().background(page).hazeSource(haze)) {
                HomeChatList(innerPadding, appViewModel, menuState, listState)
            }
        } }
        HomeContextMenu(menuState, menuHaze)
    }
}
