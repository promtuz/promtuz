package com.promtuz.chat.navigation

import androidx.compose.foundation.background
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.Modifier
import androidx.navigation3.runtime.entryProvider
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.presentation.viewmodel.WelcomeVM
import com.promtuz.chat.ui.screens.AboutScreen
import com.promtuz.chat.ui.screens.ChatAppearanceScreen
import com.promtuz.chat.ui.screens.ChatScreen
import com.promtuz.chat.ui.screens.ContactsScreen
import com.promtuz.chat.ui.screens.HomeScreen
import com.promtuz.chat.ui.screens.LogsScreen
import com.promtuz.chat.ui.screens.RelaysScreen
import com.promtuz.chat.ui.screens.SettingsScreen
import com.promtuz.chat.ui.screens.ShareIdentityScreen
import com.promtuz.chat.ui.screens.WelcomeScreen
import com.promtuz.chat.utils.extensions.fromHex
import org.koin.androidx.compose.koinViewModel


@Composable
fun AppNavigation(
    appViewModel: AppVM
) {
    val backStack = appViewModel.backStack

    NavStage(
        backStack,
        onBack = { backStack.removeLastOrNull() },
        modifier = Modifier.background(MaterialTheme.colorScheme.background),
        entryProvider = entryProvider {
            entry<Routes.App> { HomeScreen(appViewModel) }
            entry<Routes.Welcome> {
                WelcomeScreen(koinViewModel<WelcomeVM>(), onEnrolled = { appViewModel.completeOnboarding() })
            }
            entry<Routes.Chat> { key ->
                val chatVM = koinViewModel<ChatVM>()
                LaunchedEffect(key.user) { chatVM.init(key.user.fromHex()) }
                ChatScreen(key.name, chatVM)
            }
            entry<Routes.ShareIdentity> {
                ShareIdentityScreen(koinViewModel(), onScanned = { appViewModel.showInvite(it) })
            }
            entry<Routes.Contacts> { ContactsScreen() }
            entry<Routes.Settings> { SettingsScreen(appViewModel) }
            entry<Routes.ChatAppearance> { ChatAppearanceScreen() }
            entry<Routes.About> { AboutScreen() }
            entry<Routes.Logs> { LogsScreen() }
            entry<Routes.Relays> { RelaysScreen() }
        },
    )
}
