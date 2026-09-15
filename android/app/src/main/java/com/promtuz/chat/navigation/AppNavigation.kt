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
import com.promtuz.chat.ui.screens.GroupInfoScreen
import com.promtuz.chat.ui.screens.AboutScreen
import com.promtuz.chat.ui.screens.BackupRestoreScreen
import com.promtuz.chat.ui.screens.ChatAppearanceScreen
import com.promtuz.chat.ui.screens.ChatScreen
import com.promtuz.chat.ui.screens.ContactsScreen
import com.promtuz.chat.ui.screens.HomeScreen
import com.promtuz.chat.ui.screens.LogsScreen
import com.promtuz.chat.ui.screens.NotificationsSettingsScreen
import com.promtuz.chat.ui.screens.RecoveryPhraseScreen
import com.promtuz.chat.ui.screens.IdentityKeysScreen
import com.promtuz.chat.ui.screens.RelaysScreen
import com.promtuz.chat.ui.screens.RestorePhraseScreen
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
                WelcomeScreen(
                    koinViewModel<WelcomeVM>(),
                    onEnrolled = { appViewModel.completeOnboarding() },
                    onImport = { appViewModel.navigator.push(Routes.RestorePhrase) },
                )
            }
            entry<Routes.RestorePhrase> {
                RestorePhraseScreen(onRestored = { appViewModel.completeOnboarding() })
            }
            entry<Routes.IdentityKeys> {
                IdentityKeysScreen(
                    onShareIdentity = { appViewModel.navigator.push(Routes.ShareIdentity) },
                    onRecoveryPhrase = { appViewModel.navigator.push(Routes.RecoveryPhrase) },
                )
            }
            entry<Routes.RecoveryPhrase> { RecoveryPhraseScreen() }
            entry<Routes.Chat> { key ->
                val chatVM = koinViewModel<ChatVM>()
                LaunchedEffect(key.conversation) {
                    chatVM.init(key.conversation.fromHex())
                }
                ChatScreen(key.name, chatVM)
            }
            entry<Routes.GroupInfo> { key -> GroupInfoScreen(key.conversation) }
            entry<Routes.ShareIdentity> {
                ShareIdentityScreen(koinViewModel(), onScanned = { appViewModel.showInvite(it) })
            }
            entry<Routes.Contacts> { ContactsScreen(
                onScanned = { appViewModel.showInvite(it) },
                onShareIdentity = { appViewModel.navigator.push(Routes.ShareIdentity) },
            ) }
            entry<Routes.Storage> { com.promtuz.chat.ui.screens.StorageScreen(
                onOpenBackup = { appViewModel.navigator.push(Routes.BackupRestore) },
                onOpenChat = { conversation, name -> appViewModel.navigator.push(Routes.StorageChat(conversation, name)) },
            ) }
            entry<Routes.StorageChat> { key -> com.promtuz.chat.ui.screens.StorageScreen(
                conversation = key.conversation, chatName = key.name,
            ) }
            entry<Routes.Settings> { SettingsScreen(appViewModel) }
            entry<Routes.ChatAppearance> { ChatAppearanceScreen() }
            entry<Routes.About> { AboutScreen() }
            entry<Routes.NotificationsSettings> { NotificationsSettingsScreen() }
            entry<Routes.Logs> { LogsScreen() }
            entry<Routes.Relays> { RelaysScreen() }
            entry<Routes.BackupRestore> { BackupRestoreScreen() }
        },
    )
}
