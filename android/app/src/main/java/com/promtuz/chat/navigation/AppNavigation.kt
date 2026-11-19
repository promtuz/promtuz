package com.promtuz.chat.navigation

import com.promtuz.chat.security.PrivateChatSession
import com.promtuz.chat.ui.screens.ForwardScreen
import com.promtuz.chat.ui.screens.LockChatScreen
import com.promtuz.chat.ui.screens.LockedChatGate
import com.promtuz.chat.utils.extensions.fromHex
import androidx.compose.foundation.background
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.*
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
    // Profile and its editors share writes, including when Back interrupts a save.
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
            entry<Routes.RecoveryPhrase> { RecoveryPhraseScreen() }
            entry<Routes.Chat> { key ->
                val session = PrivateChatSession
                val active by session.active.collectAsState()
                session.revision.collectAsState().value
                // Process-local: a restored entry never carries a session, so it asks for the code again.
                var opened by remember(key) { mutableStateOf(key.sessionId) }
                if (opened == null && active == key.conversation) opened = session.sessionId
                val hidden = com.promtuz.core.CoreBridge.privateConversationIds().any {
                    it.contentEquals(key.conversation.fromHex())
                }
                val locked = hidden && (!appViewModel.canOpenChat(key.conversation) || active != key.conversation ||
                        opened == null || opened != session.sessionId)
                androidx.compose.foundation.layout.Box {
                    if (!locked) {
                        // A locked chat's model wipes itself when its session ends, so each unlock gets a new one.
                        val chatVM = koinViewModel<ChatVM>(key = "chat:${key.conversation}:${if (hidden) opened else "open"}",
                            parameters = { parametersOf(key.conversation) })
                        LaunchedEffect(chatVM) {
                            chatVM.init()
                        }
                        ChatScreen(key.name, chatVM)
                    }
                    // The fade waits for the chat's first, slow frame so it isn't swallowed by it.
                    var covering by remember(key) { mutableStateOf(locked) }
                    LaunchedEffect(locked) {
                        if (!locked) { withFrameNanos {}; withFrameNanos {} }
                        covering = locked
                    }
                    // Locking again covers the chat at once.
                    androidx.compose.animation.AnimatedVisibility(
                        locked || covering,
                        enter = androidx.compose.animation.EnterTransition.None,
                        exit = androidx.compose.animation.fadeOut(androidx.compose.animation.core.tween(420,
                            easing = androidx.compose.animation.core.EaseOutQuint)),
                    ) {
                        LockedChatGate(appViewModel, key.conversation, key.name) { opened = it }
                    }
                }
            }
            entry<Routes.ContactCard> { key -> ContactCardScreen(key) }
            entry<Routes.MessageRequests> { MessageRequestsScreen() }
            entry<Routes.PrivacySettings> { PrivacySettingsScreen(appViewModel.navigator) }
            entry<Routes.BlockedPeople> { BlockedPeopleScreen() }
            entry<Routes.ResetLockedChats> { ResetLockedChatsScreen() }
            entry<Routes.ContactInfo> { key -> ContactInfoScreen(key.conversation, key.peer) }
            entry<Routes.PersonInfo> { key -> ContactInfoScreen(null, key.peer, key.name) }
            entry<Routes.SharedMedia> { key -> SharedMediaScreen(key.conversation, key.name) }
            entry<Routes.Forward> { key -> ForwardScreen(key, appViewModel) }
            entry<Routes.GroupInfo> { key -> GroupInfoScreen(key.conversation) }
            entry<Routes.GroupSettings> { key -> GroupSettingsScreen(key.conversation) }
            entry<Routes.ShareIdentity> {
                ShareIdentityScreen(koinViewModel(), koinViewModel<ProfileVM>(viewModelStoreOwner = profileOwner), onScanned = { appViewModel.showInvite(it) })
            }
            entry<Routes.Contacts> { ContactsScreen(
                onScanned = { appViewModel.showInvite(it) },
                onShareIdentity = { appViewModel.navigator.push(Routes.ShareIdentity) },
            ) }
            entry<Routes.Storage> { StorageScreen(
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
            entry<Routes.LockChat> { key ->
                LockChatScreen(
                    conversation = key.conversation,
                    onBack = { if (backStack.lastOrNull() == key) backStack.removeLastOrNull() },
                    onLocked = {
                        appViewModel.clearPrivateSummary()
                        appViewModel.navigator.reset(Routes.App)
                    },
                    hide = key.hide,
                    onHidden = { if (backStack.lastOrNull() == key) backStack.removeLastOrNull() },
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
            entry<Routes.Settings> {
                SettingsScreen(appViewModel, koinViewModel<ProfileVM>(viewModelStoreOwner = profileOwner))
            }
            entry<Routes.ChatAppearance> { ChatAppearanceScreen() }
            entry<Routes.About> { AboutScreen(
                onOpenLicenses = { appViewModel.navigator.push(Routes.OpenSourceLicenses) },
                onOpenHistory = { appViewModel.navigator.push(Routes.ReleaseHistory) },
            ) }
            entry<Routes.Updates> { UpdateScreen() }
            entry<Routes.ReleaseHistory> { ReleaseHistoryScreen() }
            entry<Routes.OpenSourceLicenses> { OpenSourceLicensesScreen(
                onLibraryClick = { appViewModel.navigator.push(Routes.LibraryLicense(it)) },
            ) }
            entry<Routes.LibraryLicense> { key -> LibraryLicenseScreen(key.id) }
            entry<Routes.NotificationsSettings> { NotificationsSettingsScreen(appViewModel.navigator) }
            entry<Routes.NotificationsKind> { key -> NotificationsKindScreen(key.groups, appViewModel::openChat) }
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
