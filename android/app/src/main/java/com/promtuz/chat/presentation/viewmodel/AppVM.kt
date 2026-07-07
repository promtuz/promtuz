package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.content.Context
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import androidx.navigation3.runtime.NavBackStack
import androidx.navigation3.runtime.NavKey
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.Chat
import com.promtuz.chat.domain.model.LastMessage
import com.promtuz.chat.navigation.AppNavigator
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.state.InviteSheet
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import timber.log.Timber
import com.promtuz.chat.presentation.state.ConnectionState as CS

class AppVM(
    private val application: Application, private val bridge: CoreBridge
) : ViewModel() {
    private val context: Context get() = application.applicationContext

    var activeChatUser: Chat? = null

    var backStack = NavBackStack<NavKey>(if (CoreBridge.shouldLaunchApp()) Routes.App else Routes.Welcome)
    val navigator = AppNavigator(backStack)

    /** Invite that arrived before onboarding finished; raised once enroll completes. */
    var pendingInvite: ByteArray? = null

    private val _dynamicTitle = MutableStateFlow(context.resources.getString(R.string.app_name))
    val dynamicTitle: StateFlow<String> = _dynamicTitle.asStateFlow()

    private val _chats = MutableStateFlow(emptyList<Chat>())
    val chats: StateFlow<List<Chat>> = _chats.asStateFlow()

    private val _chatsLoading = MutableStateFlow(false)
    val chatsLoading: StateFlow<Boolean> = _chatsLoading.asStateFlow()

    /** Invite-link confirmation sheet; null when hidden. Driven by deeplinks. */
    private val _invite = MutableStateFlow<InviteSheet?>(null)
    val invite: StateFlow<InviteSheet?> = _invite.asStateFlow()

    init {
        viewModelScope.launch {
            var titleResetJob: Job? = null

            bridge.connection.collect { state ->
                    titleResetJob?.cancel()

                    _dynamicTitle.value = when (state) {
                        CS.Idle -> context.resources.getString(R.string.app_name)
                        CS.Connecting, CS.Failed, CS.Handshaking, CS.Reconnecting, CS.Resolving, CS.NoInternet -> context.resources.getString(
                            state.text
                        )

                        CS.Connected, CS.Disconnected -> {
                            context.resources.getString(state.text).also {
                                titleResetJob = launch {
                                    delay(1200)
                                    _dynamicTitle.value =
                                        context.resources.getString(R.string.app_name)
                                }
                            }
                        }
                    }
                }
        }

        // Listen for incoming messages to update chat list
        viewModelScope.launch {
            bridge.messageEvents.collect {
                refreshChats()
            }
        }

        refreshChats()
    }

    companion object {
        private const val TAG = "AppVM"
        private val log = { Timber.tag(TAG) }
    }

    fun openChat(identityKey: Chat) {
        activeChatUser = identityKey
        navigator.push(Routes.Chat)
    }

    /** A `/pair` deeplink arrived: decode it and raise the confirmation sheet. */
    fun showInvite(bytes: ByteArray) {
        _invite.value = InviteSheet.Decoding
        viewModelScope.launch {
            _invite.value = try {
                val p = bridge.previewInvite(bytes)
                InviteSheet.Confirm(bytes, p.ipk, p.name, p.alreadyContact, p.expired)
            } catch (e: Exception) {
                Timber.tag(TAG).w(e, "previewInvite failed")
                InviteSheet.Invalid
            }
        }
    }

    /** User tapped [Add]: queue the pairing (Ok != paired) and show brief success. */
    fun acceptInvite(bytes: ByteArray, name: String) {
        viewModelScope.launch {
            try {
                bridge.pairFromQr(bytes)
                _invite.value = InviteSheet.Added(name)
                refreshChats()
            } catch (e: Exception) {
                Timber.tag(TAG).w(e, "pairFromQr failed")
                _invite.value = InviteSheet.Invalid
            }
        }
    }

    fun dismissInvite() {
        _invite.value = null
    }

    /** Enroll finished: drop Welcome from the stack (no going back) and raise any deferred invite. */
    fun completeOnboarding() {
        navigator.reset(Routes.App)
        pendingInvite?.let { showInvite(it); pendingInvite = null }
    }

    fun refreshChats() {
        viewModelScope.launch {
            _chatsLoading.value = true
            try {
                val contacts = bridge.contacts()
                val conversations = bridge.conversations()

                // Build a map of peer_ipk -> latest message
                val convMap = conversations.associateBy { it.peerIpk.toList() }

                _chats.value = contacts.map { contact ->
                    val lastMsg = convMap[contact.ipk.toList()]
                    Chat(
                        identity = contact.ipk,
                        nickname = contact.name,
                        lastMessage = if (lastMsg != null) {
                            LastMessage(lastMsg.content, lastMsg.timestamp.toLong() * 1000)
                        } else {
                            LastMessage(null, contact.addedAt.toLong() * 1000)
                        }
                    )
                }.sortedByDescending { it.lastMessage.timestamp }
            } catch (e: Exception) {
                Timber.tag(TAG).e(e, "Failed to load chats")
            }
            _chatsLoading.value = false
        }
    }
}
