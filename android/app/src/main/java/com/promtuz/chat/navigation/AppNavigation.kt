package com.promtuz.chat.navigation

import androidx.compose.animation.SizeTransform
import androidx.compose.foundation.background
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.lifecycle.viewmodel.navigation3.rememberViewModelStoreNavEntryDecorator
import androidx.navigation3.runtime.entryProvider
import androidx.navigation3.runtime.rememberSaveableStateHolderNavEntryDecorator
import androidx.navigation3.ui.NavDisplay
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.constants.Naviganimation
import com.promtuz.chat.ui.screens.AboutScreen
import com.promtuz.chat.ui.screens.HomeScreen
import com.promtuz.chat.ui.screens.LogsScreen
import com.promtuz.chat.ui.screens.NetworkStatsScreen
import com.promtuz.chat.ui.screens.SavedUsersScreen
import com.promtuz.chat.ui.screens.SettingsScreen


@Composable
fun AppNavigation(
    appViewModel: AppVM
) {
    val backStack = appViewModel.backStack

    NavDisplay(
        backStack,
        onBack = { backStack.removeLastOrNull() },
        modifier = Modifier
            .background(MaterialTheme.colorScheme.background),
        entryDecorators = listOf(
            rememberSaveableStateHolderNavEntryDecorator(),
            rememberViewModelStoreNavEntryDecorator()
        ),
        entryProvider = entryProvider {
            entry<Routes.App> { HomeScreen(appViewModel) }
            //entry<Routes.Chat> { key -> ChatScreen(appViewModel) }
            entry<Routes.SavedUsers> { SavedUsersScreen() }
            entry<Routes.Settings> { SettingsScreen(appViewModel) }
            entry<Routes.About> { AboutScreen() }
            entry<Routes.Logs> { LogsScreen() }
            entry<Routes.NetworkStats> { NetworkStatsScreen() }
        },
        sizeTransform = SizeTransform(clip = false),
        transitionSpec = { Naviganimation.transitionSpec() },
        popTransitionSpec = { Naviganimation.popTransitionSpec() }
    )

}