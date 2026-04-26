package com.promtuz.chat.ui.screens

import androidx.compose.material3.Scaffold
import androidx.compose.runtime.Composable
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.components.HomeChatList
import com.promtuz.chat.ui.components.HomeFab
import com.promtuz.chat.ui.components.HomeTopBar

@androidx.annotation.OptIn(androidx.camera.core.ExperimentalGetImage::class)
@Composable
fun HomeScreen(
    appViewModel: AppVM
) {
    Scaffold(
        topBar = { HomeTopBar(appViewModel) },
        floatingActionButton = { HomeFab(appViewModel) }
    ) { innerPadding ->
        HomeChatList(innerPadding, appViewModel)
    }
}
