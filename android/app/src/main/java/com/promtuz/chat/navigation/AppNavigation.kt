package com.promtuz.chat.navigation

import com.promtuz.chat.utils.extensions.fromHex
import androidx.compose.foundation.background
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.Modifier
import androidx.navigation3.runtime.entryProvider
import androidx.lifecycle.viewmodel.compose.LocalViewModelStoreOwner
import com.promtuz.chat.presentation.viewmodel.ProfileVM
import com.promtuz.chat.ui.screens.ProfileScreen
import com.promtuz.chat.ui.screens.ProfilePhotoScreen
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.presentation.viewmodel.WelcomeVM
import com.promtuz.chat.ui.screens.*
import org.koin.androidx.compose.koinViewModel
import org.koin.core.parameter.parametersOf


@Composable
fun AppNavigation(
    appViewModel: AppVM
) {
    val backStack = appViewModel.backStack
    // Profile and its photo editor share writes, including when Back interrupts a save.
    val profileOwner = checkNotNull(LocalViewModelStoreOwner.current)

    NavStage(
        backStack,
        onBack = { backStack.removeLastOrNull() },
        modifier = Modifier.background(MaterialTheme.colorScheme.background),
        entryProvider = entryProvider {
            entry<Routes.App> { HomeScreen(appViewModel) }
            entry<Routes.Search> { HomeSearchScreen(appViewModel) }
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
                val hidden = com.promtuz.core.CoreBridge.privateConversationIds().any {
                    it.contentEquals(key.conversation.fromHex())
                }
                val session = com.promtuz.chat.security.PrivateChatSession
                if (!appViewModel.canOpenChat(key.conversation) ||
                    (hidden && (session.active.value != key.conversation || key.sessionId == null || key.sessionId != session.sessionId))) {
                    LaunchedEffect(key) { appViewModel.navigator.reset(Routes.App) }
                    return@entry
                }
                val chatVM = koinViewModel<ChatVM>(parameters = { parametersOf(key.conversation) })
                LaunchedEffect(key.conversation) {
                    chatVM.init()
                }
                ChatScreen(key.name, chatVM)
            }
            entry<Routes.ContactCard> { key -> ContactCardScreen(key) }
            entry<Routes.MessageRequests> { MessageRequestsScreen() }
            entry<Routes.PrivacySettings> { PrivacySettingsScreen() }
            entry<Routes.ResetLockedChats> { ResetLockedChatsScreen() }
            entry<Routes.ContactInfo> { key -> ContactInfoScreen(key.conversation, key.peer) }
            entry<Routes.PersonInfo> { key -> ContactInfoScreen(null, key.peer, key.name) }
            entry<Routes.SharedMedia> { key -> SharedMediaScreen(key.conversation, key.name) }
            entry<Routes.GroupInfo> { key -> GroupInfoScreen(key.conversation) }
            entry<Routes.GroupSettings> { key -> GroupSettingsScreen(key.conversation) }
            entry<Routes.ShareIdentity> {
                ShareIdentityScreen(koinViewModel(), onScanned = { appViewModel.showInvite(it) })
            }
            entry<Routes.Contacts> { ContactsScreen(
                onScanned = { appViewModel.showInvite(it) },
                onShareIdentity = { appViewModel.navigator.push(Routes.ShareIdentity) },
            ) }
            entry<Routes.Storage> { StorageScreen(
                onOpenBackup = { appViewModel.navigator.push(Routes.BackupRestore) },
                onOpenChat = { conversation, name -> appViewModel.navigator.push(Routes.StorageChat(conversation, name)) },
            ) }
            entry<Routes.StorageChat> { key -> StorageScreen(
                conversation = key.conversation, chatName = key.name,
            ) }
            entry<Routes.Profile> {
                ProfileScreen(
                    viewModel = koinViewModel<ProfileVM>(viewModelStoreOwner = profileOwner),
                    onChoosePhoto = { appViewModel.navigator.push(Routes.ProfilePhoto(it.toString())) },
                )
            }
            entry<Routes.ProfilePhoto> { key ->
                ProfilePhotoScreen(
                    uri = key.uri,
                    viewModel = if (key.group == null) koinViewModel<ProfileVM>(viewModelStoreOwner = profileOwner)
                        else androidx.lifecycle.viewmodel.compose.viewModel(viewModelStoreOwner = profileOwner, key = "group-photo:${key.group}") { ProfileVM(key.group) },
                    onSaved = { if (backStack.lastOrNull() == key) backStack.removeLastOrNull() },
                )
            }
            entry<Routes.Settings> { SettingsScreen(appViewModel) }
            entry<Routes.ChatAppearance> { ChatAppearanceScreen() }
            entry<Routes.About> { AboutScreen(
                onOpenLicenses = { appViewModel.navigator.push(Routes.OpenSourceLicenses) },
            ) }
            entry<Routes.Updates> { UpdateScreen() }
            entry<Routes.OpenSourceLicenses> { OpenSourceLicensesScreen(
                onLibraryClick = { appViewModel.navigator.push(Routes.LibraryLicense(it)) },
            ) }
            entry<Routes.LibraryLicense> { key -> LibraryLicenseScreen(key.id) }
            entry<Routes.NotificationsSettings> { NotificationsSettingsScreen() }
            entry<Routes.Logs> { LogsScreen() }
            entry<Routes.Relays> { RelaysScreen() }
            entry<Routes.BackupRestore> { BackupRestoreScreen() }
            entry<Routes.Stickers> {
                StickersScreen(
                    onCreate = { appViewModel.navigator.push(Routes.NewStickerPack()) },
                    onAddImages = { pack -> appViewModel.navigator.push(Routes.NewStickerPack(pack)) },
                )
            }
            entry<Routes.NewStickerPack> { key ->
                NewStickerPackScreen(packHex = key.pack, onDone = { backStack.removeLastOrNull() })
            }
        },
    )
}
