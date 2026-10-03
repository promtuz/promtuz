package com.promtuz.chat.ui.screens

import androidx.compose.foundation.layout.Box
import androidx.compose.material3.Scaffold
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
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
    Box {
        Scaffold(
            topBar = { HomeTopBar(appViewModel) },
            floatingActionButton = { HomeFab(appViewModel) }
        ) { innerPadding ->
            HomeChatList(innerPadding, appViewModel, menuState)
        }
        HomeContextMenu(menuState)
    }
}
