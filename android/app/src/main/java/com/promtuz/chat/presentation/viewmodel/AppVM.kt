package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.content.Context
import android.widget.Toast
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import androidx.navigation3.runtime.NavBackStack
import androidx.navigation3.runtime.NavKey
import com.promtuz.chat.R
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.domain.model.ChatSummary
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.domain.model.systemContent
import com.promtuz.chat.navigation.AppNavigator
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.state.InviteSheet
import com.promtuz.chat.security.RecoveryStore
import com.promtuz.chat.ui.components.BubbleTextLayouts
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.reason
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch
import timber.log.Timber
import uniffi.core.MessageRecord
import com.promtuz.chat.presentation.state.ConnectionState as CS

class AppVM(
    private val application: Application, private val bridge: CoreBridge
) : ViewModel() {
    private val context: Context get() = application.applicationContext

    var backStack = NavBackStack<NavKey>(if (CoreBridge.shouldLaunchApp()) Routes.App else Routes.Welcome)
    val navigator = AppNavigator(backStack)
    private val _messageToShow = MutableStateFlow<com.promtuz.chat.domain.model.MessageLocation?>(null)
    val messageToShow = _messageToShow.asStateFlow()

    fun showMessage(message: com.promtuz.chat.domain.model.MessageLocation) {
        _messageToShow.value = message
        // Reuse the existing chat entry and its scroll/draft state when it is already in the stack.
        val route = backStack.filterIsInstance<Routes.Chat>().lastOrNull { it.conversation == message.conversation }
            ?: Routes.Chat(message.conversation, message.chatName)
        navigator.openExternal(route)
    }

    fun consumeMessage(message: com.promtuz.chat.domain.model.MessageLocation) {
        _messageToShow.compareAndSet(message, null)
    }

    var pendingInvite: ByteArray? = null
    var pendingContactCard: String? = null

    private val _dynamicTitle = MutableStateFlow(context.resources.getString(R.string.app_name))
    val dynamicTitle: StateFlow<String> = _dynamicTitle.asStateFlow()

    /** Requests included. Core orders it, pinned first, so the list keeps no ordering rule of its own. */
    val chats: StateFlow<List<ChatSummary>> =
        observeQuery(setOf("contacts", "messages", "conversations", "conversation_members")) {
            loadSummaries()
        }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), emptyList())

    /** Latest unread arrival not yet exposed in the Home viewport. */
    val unseenOnHome = observeQuery(setOf("messages", "prefs")) {
        bridge.unreadCounts().mapNotNull { unread ->
            val conv = unread.conversationId.toHex()
            val newest = bridge.recentIncoming(unread.conversationId, 1).lastOrNull()?.id
            if (newest != null && bridge.pref("home_seen:$conv") != newest) conv to newest else null
        }.toMap()
    }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), emptyMap())

    fun sawHomeRows(conversations: Set<String>) {
        val arrivals = unseenOnHome.value.filterKeys { it in conversations }
        if (arrivals.isEmpty()) return
        viewModelScope.launch {
            // Capture the exact IDs shown; an arrival during this write stays unseen.
            arrivals.forEach { (conv, id) -> bridge.setPref("home_seen:$conv", id) }
        }
    }

    val presenceByPeer: StateFlow<Map<String, Presence>> get() = bridge.presenceByPeer

    /** Keyed by conversation, since a contact can type in a group without typing in your DM. */
    internal val conversationActivity = ConversationActivity(viewModelScope, ACTIVITY_TTL_MS)
    val activityByChat: StateFlow<Map<String, Int>> = conversationActivity.byChat

    private val _invite = MutableStateFlow<InviteSheet?>(null)
    val invite: StateFlow<InviteSheet?> = _invite.asStateFlow()

    init {
        if (!CoreBridge.shouldLaunchApp()) viewModelScope.launch {
            if (RecoveryStore.tryAutoRestore(context)) completeOnboarding()
        }

        // Each subscribe replaces the connection's whole set, so this is the only subscriber
        // and it resends every contact on each connect and contact change.
        viewModelScope.launch {
            combine(
                bridge.connection.filter { it == CS.Connected },
                observeQuery(setOf("contacts")) { bridge.contacts().map { it.ipk } },
            ) { _, ipks -> ipks }
                .collect { ipks -> runCatching { bridge.subscribePresence(ipks) } }
        }

        // Each member times out, since an offline peer never sends a stop.
        viewModelScope.launch {
            bridge.activity.collect { sig ->
                conversationActivity.update(sig.conversation.toHex(), sig.peer.toHex(), sig.bits)
            }
        }

        viewModelScope.launch {
            var titleResetJob: Job? = null

            bridge.connection.collect { state ->
                    titleResetJob?.cancel()

                    _dynamicTitle.value = when (state) {
                        CS.Idle -> context.resources.getString(R.string.app_name)
                        // Held until the next state replaces it (Syncing → Connected).
                        CS.Disconnected, CS.Connecting, CS.Failed, CS.Handshaking, CS.Resolving, CS.Syncing -> context.resources.getString(
                            state.text
                        )

                        CS.Connected -> {
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

    }

    companion object {
        private const val TAG = "AppVM"

        private const val ACTIVITY_TTL_MS = 6_000L
    }

    fun openChat(conversationHex: String, name: String) {
        navigator.push(Routes.Chat(conversationHex, name))
    }

    fun openChatWith(peerHex: String, name: String) = viewModelScope.launch {
        val conv = runCatching { bridge.conversationWith(peerHex.fromHex()) }.getOrNull() ?: return@launch
        navigator.push(Routes.Chat(conv.toHex(), name))
    }

    fun markConversationRead(conversationHex: String) = viewModelScope.launch {
        runCatching { bridge.markConversationRead(conversationHex.fromHex()) }
    }

    /** A direct chat forgets the contact too, since half-forgetting one leaves a phantom row.
     *  [onDone] runs only once the chat is really gone. */
    fun deleteChat(summary: ChatSummary, onDone: () -> Unit = {}) = viewModelScope.launch {
        val result = if (summary.isGroup) {
            runCatching { bridge.deleteConversation(summary.conversationHex.fromHex()) }
        } else {
            summary.peerHex?.let { runCatching { bridge.forgetContact(it.fromHex()) } }
                ?: Result.success(Unit)
        }
        result.onSuccess { onDone() }
            .onFailure { complain(it, "Couldn't delete this chat") }
    }

    fun clearHistory(conversationHex: String) = viewModelScope.launch {
        runCatching { bridge.clearConversationHistory(conversationHex.fromHex()) }
            .onFailure { complain(it, "Couldn't clear this chat") }
    }

    /** The home list has no error state, and a tap that silently does nothing reads as a broken app. */
    private fun complain(e: Throwable, fallback: String) {
        val why = e.reason(fallback)
        Timber.tag(TAG).e(e, "$fallback: $why")
        Toast.makeText(context, why, Toast.LENGTH_LONG).show()
    }

    /** Leaving needs the network and can fail; [onDone] runs only when both steps succeed. */
    fun leaveAndDelete(summary: ChatSummary, onDone: () -> Unit = {}) = viewModelScope.launch {
        val conv = summary.conversationHex.fromHex()
        runCatching { bridge.leaveGroup(conv) }
            .onFailure { complain(it, "Couldn't leave this group") }
            .onSuccess {
                runCatching { bridge.deleteConversation(conv) }
                    .onSuccess { onDone() }
                    .onFailure { complain(it, "Left the group, but the chat wouldn't delete") }
            }
    }

    fun showInvite(bytes: ByteArray) {
        _invite.value = InviteSheet.Decoding
        viewModelScope.launch {
            _invite.value = try {
                val p = bridge.previewInvite(bytes)
                InviteSheet.Confirm(bytes, p.ipk, p.name, p.alreadyContact, p.expiryMs.toLong())
            } catch (e: Exception) {
                Timber.tag(TAG).w(e, "previewInvite failed")
                InviteSheet.Invalid()
            }
        }
    }

    /** Core saves the contact only after the welcome publishes, so its arrival is the success
     *  signal; nothing within the window means the peer is unreachable. */
    fun acceptInvite(bytes: ByteArray, ipk: ByteArray, name: String) {
        _invite.value = InviteSheet.Pairing(name)
        viewModelScope.launch {
            try {
                bridge.pairFromQr(bytes)
            } catch (e: Exception) {
                // A synchronous refusal, such as pairing with ourselves.
                Timber.tag(TAG).w(e, "pairFromQr failed")
                _invite.value = InviteSheet.Invalid(e.message ?: "Couldn't start pairing.")
                return@launch
            }
            val appeared = withTimeoutOrNull(12_000) {
                while (bridge.contacts().none { it.ipk.contentEquals(ipk) }) delay(400)
                true
            } ?: false
            _invite.value =
                if (appeared) InviteSheet.Added(ipk, name) else InviteSheet.Unreachable(bytes, name)
        }
    }

    fun dismissInvite() {
        _invite.value = null
    }

    fun completeOnboarding() {
        navigator.reset(Routes.App)
        pendingInvite?.let { showInvite(it); pendingInvite = null }
        pendingContactCard?.let { navigator.openExternal(Routes.ContactCard(encoded = it)); pendingContactCard = null }
    }

    private suspend fun loadSummaries(): List<ChatSummary> = try {
        val contactByIpk = bridge.contacts().associateBy { it.ipk.toList() }
        val lastByConv = bridge.conversations().associateBy { it.conversationId.toList() }
        val unread = bridge.unreadCounts().associate { it.conversationId.toList() to it.count.toInt() }

        bridge.listConversations().mapNotNull { c ->
            val key = c.id.toList()
            val last = lastByConv[key]
            val contact = c.peer?.let { contactByIpk[it.toList()] }
            // Opened to message someone, then left without writing anything.
            if (c.kind.toInt() == 0 && contact == null && !c.request && last == null) return@mapNotNull null
            ChatSummary(
                conversationHex = c.id.toHex(),
                name = if (c.kind.toInt() == 1 || c.request) c.displayName
                       else contact?.name.orEmpty(),
                kind = c.kind.toInt(),
                peerHex = c.peer?.toHex(),
                memberCount = c.members.size,
                lastPreview = last?.let { preview(c.id, it) },
                lastMessageId = last?.id,
                lastDispatchId = last?.dispatchId?.toHex(),
                lastMediaKind = last?.mediaKind?.toInt() ?: 0,
                timestampMs = (last?.timestamp ?: c.createdAt).toLong() * 1000,
                status = contact?.status?.toInt() ?: 1,
                rejectReason = contact?.rejectReason?.toInt(),
                unreadCount = unread[key] ?: 0,
                // A membership line already names who did it.
                lastOutgoing = last?.outgoing == true && last.system.toInt() !in setOf(1, 2, 3, 4, 6, 7),
                lastDeleted = last?.deleted == true,
                lastStatus = last?.status?.toInt() ?: 1,
                pinned = c.pinned,
                muted = c.muted,
                amMember = c.amMember,
                canLeave = c.canLeave,
                ownerIsStuck = c.ownerIsStuck,
                commits = c.commits,
                rawTitle = c.title,
                request = c.request,
            )
        }
    } catch (e: Exception) {
        Timber.tag(TAG).e(e, "Failed to load chats")
        emptyList()
    }

    private suspend fun preview(conversation: ByteArray, last: MessageRecord): String =
        when (val code = last.system.toInt()) {
            0 -> last.content
            5 -> "Call"
            else -> {
                val names = bridge.members(conversation).associate { it.ipk.toHex() to if (it.me) "You" else it.name }
                BubbleTextLayouts.systemLine(systemContent(code, last.senderIpk?.toHex(), last.content, names))
            }
        }
}
