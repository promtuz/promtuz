package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.content.Context
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import androidx.navigation3.runtime.NavBackStack
import androidx.navigation3.runtime.NavKey
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.Chat
import com.promtuz.chat.domain.model.ContactData
import com.promtuz.chat.domain.model.LastMessage
import com.promtuz.chat.domain.model.MessageData
import com.promtuz.chat.navigation.AppNavigator
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.security.KeyManager
import com.promtuz.chat.utils.serialization.AppCbor
import com.promtuz.core.API
import com.promtuz.core.events.InternalEvents
import com.promtuz.core.events.MessageEvent
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.filterIsInstance
import kotlinx.coroutines.launch
import kotlinx.serialization.ExperimentalSerializationApi
import kotlinx.serialization.decodeFromByteArray
import timber.log.Timber
import com.promtuz.chat.presentation.state.ConnectionState as CS

class AppVM(
    private val application: Application, private val keyManager: KeyManager, private val api: API
) : ViewModel() {
    private val context: Context get() = application.applicationContext

    var activeChatUser: Chat? = null

    var backStack = NavBackStack<NavKey>(Routes.App)
    val navigator = AppNavigator(backStack)

    private val _dynamicTitle = MutableStateFlow(context.resources.getString(R.string.app_name))
    val dynamicTitle: StateFlow<String> = _dynamicTitle.asStateFlow()

    private val _chats = MutableStateFlow(emptyList<Chat>())
    val chats: StateFlow<List<Chat>> = _chats.asStateFlow()

    private val _chatsLoading = MutableStateFlow(false)
    val chatsLoading: StateFlow<Boolean> = _chatsLoading.asStateFlow()

    init {
        viewModelScope.launch {
            var titleResetJob: Job? = null

            api.eventsFlow.filterIsInstance<InternalEvents.ConnectionEv>().distinctUntilChanged().collect { state ->
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
            api.eventsFlow.filterIsInstance<MessageEvent>().collect {
                refreshChats()
            }
        }
    }

    companion object {
        private const val TAG = "AppVM"
        private val log = { Timber.tag(TAG) }
    }

    var connecting = false

    fun openChat(identityKey: Chat) {
        activeChatUser = identityKey
        navigator.push(Routes.Chat)
    }

    init {
        api.initApi(context)

        connection()
        refreshChats()
    }

    fun connection() {
        if (connecting) return

        viewModelScope.launch {
            api.connect(context)
        }
    }

    @OptIn(ExperimentalSerializationApi::class)
    fun refreshChats() {
        viewModelScope.launch {
            _chatsLoading.value = true
            try {
                val contactBytes = api.getContacts()
                val contacts = AppCbor.instance.decodeFromByteArray<List<ContactData>>(contactBytes)

                val convBytes = api.getConversations()
                val conversations = AppCbor.instance.decodeFromByteArray<List<MessageData>>(convBytes)

                // Build a map of peer_ipk -> latest message
                val convMap = conversations.associateBy { it.peer_ipk.toList() }

                _chats.value = contacts.map { contact ->
                    val lastMsg = convMap[contact.ipk.toList()]
                    Chat(
                        identity = contact.ipk,
                        nickname = contact.name,
                        lastMessage = if (lastMsg != null) {
                            LastMessage(lastMsg.content, lastMsg.timestamp * 1000)
                        } else {
                            LastMessage(null, contact.added_at * 1000)
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
