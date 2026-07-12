package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.content.Context
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import androidx.navigation3.runtime.NavBackStack
import androidx.navigation3.runtime.NavKey
import com.promtuz.chat.R
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.domain.model.ChatSummary
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.navigation.AppNavigator
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.state.InviteSheet
import com.promtuz.chat.security.RecoveryStore
import com.promtuz.chat.utils.extensions.fromHex
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
import com.promtuz.chat.presentation.state.ConnectionState as CS

class AppVM(
    private val application: Application, private val bridge: CoreBridge
) : ViewModel() {
    private val context: Context get() = application.applicationContext

    var backStack = NavBackStack<NavKey>(if (CoreBridge.shouldLaunchApp()) Routes.App else Routes.Welcome)
    val navigator = AppNavigator(backStack)

    /** Invite that arrived before onboarding finished; raised once enroll completes. */
    var pendingInvite: ByteArray? = null

    private val _dynamicTitle = MutableStateFlow(context.resources.getString(R.string.app_name))
    val dynamicTitle: StateFlow<String> = _dynamicTitle.asStateFlow()

    /** Home chat list — reactive: re-reads on contact/message changes, re-sorts
     *  pinned-first the instant a pin toggles. */
    val chats: StateFlow<List<ChatSummary>> =
        combine(
            observeQuery(setOf("contacts", "messages")) { loadSummaries() },
            ChatPrefs.pinned,
        ) { list, pinned ->
            list.sortedWith(
                compareByDescending<ChatSummary> { it.peerHex in pinned }
                    .thenByDescending { it.timestampMs }
            )
        }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), emptyList())

    /** Live presence per contact (hex IPK) for the whole app — home dots + chat header. */
    val presenceByPeer: StateFlow<Map<String, Presence>> get() = bridge.presenceByPeer

    /** Live activity bits per contact (hex IPK), timed out client-side; 0/absent = quiet. */
    private val _activityByPeer = MutableStateFlow<Map<String, Int>>(emptyMap())
    val activityByPeer: StateFlow<Map<String, Int>> = _activityByPeer.asStateFlow()
    private val activityExpiry = mutableMapOf<String, Job>()

    /** Invite-link confirmation sheet; null when hidden. Driven by deeplinks. */
    private val _invite = MutableStateFlow<InviteSheet?>(null)
    val invite: StateFlow<InviteSheet?> = _invite.asStateFlow()

    init {
        // Channel A silent restore (IDENTITY_RECOVERY.md §5.1): fresh install
        // with a Block Store hit re-adopts the identity + imports the
        // Auto-Backup blob, then skips onboarding entirely.
        if (!CoreBridge.shouldLaunchApp()) viewModelScope.launch {
            if (RecoveryStore.tryAutoRestore(context)) completeOnboarding()
        }

        // Presence is app-wide, not per-chat: subscribe to the WHOLE contact set so
        // the home list and every open chat see live status. The relay scopes a
        // subscription to the connection and treats each SubscribePresence as a
        // full-set replace, so re-express the entire set on each (re)connect and
        // whenever a contact is added. ChatVM must NOT also subscribe — one owner
        // keeps the replace-semantics from narrowing us back to a single peer.
        viewModelScope.launch {
            combine(
                bridge.connection.filter { it == CS.Connected },
                observeQuery(setOf("contacts")) { bridge.contacts().map { it.ipk } },
            ) { _, ipks -> ipks }
                .collect { ipks -> runCatching { bridge.subscribePresence(ipks) } }
        }

        // Typing/recording already reaches us for any contact (relay-routed,
        // surfaced view-agnostically) — it just wasn't collected outside a chat.
        // Track it app-wide for the home list; time each peer out (an offline
        // peer never sends "stopped").
        viewModelScope.launch {
            bridge.activity.collect { sig ->
                val hex = sig.peer.toHex()
                activityExpiry.remove(hex)?.cancel()
                if (sig.bits != 0) {
                    _activityByPeer.value = _activityByPeer.value + (hex to sig.bits)
                    activityExpiry[hex] = viewModelScope.launch {
                        delay(ACTIVITY_TTL_MS)
                        activityExpiry.remove(hex)
                        _activityByPeer.value = _activityByPeer.value - hex
                    }
                } else {
                    _activityByPeer.value = _activityByPeer.value - hex
                }
            }
        }

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

    }

    companion object {
        private const val TAG = "AppVM"
        private val log = { Timber.tag(TAG) }

        /** Client-side typing/recording timeout; matches ChatVM's TTL. */
        private const val ACTIVITY_TTL_MS = 6_000L
    }

    fun openChat(peerHex: String, name: String) {
        navigator.push(Routes.Chat(peerHex, name))
    }

    /** Home-list "Mark read": clear the unread backlog for this conversation. */
    fun markConversationRead(peerHex: String) = viewModelScope.launch {
        runCatching { bridge.markConversationRead(peerHex.fromHex()) }
    }

    /** Home-list "Delete chat": forget the contact + all local state, drop its flags. */
    fun deleteChat(peerHex: String) = viewModelScope.launch {
        runCatching { bridge.forgetContact(peerHex.fromHex()) }
        ChatPrefs.forget(peerHex)
    }

    /** A `/pair` deeplink arrived: decode it and raise the confirmation sheet. */
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

    /** User tapped Add: queue the pairing, then WATCH for the contact to appear.
     *  pair() saves it PENDING only after the welcome publishes, so its arrival
     *  is our success signal; nothing within the window means unreachable
     *  (their KeyPackage isn't published — the common "new user" case). */
    fun acceptInvite(bytes: ByteArray, ipk: ByteArray, name: String) {
        _invite.value = InviteSheet.Pairing(name)
        viewModelScope.launch {
            try {
                bridge.pairFromQr(bytes)
            } catch (e: Exception) {
                // Synchronous refusal (self-pair) — surface the reason directly.
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

    /** Enroll finished: drop Welcome from the stack (no going back) and raise any deferred invite. */
    fun completeOnboarding() {
        navigator.reset(Routes.App)
        pendingInvite?.let { showInvite(it); pendingInvite = null }
    }

    private suspend fun loadSummaries(): List<ChatSummary> = try {
        val contacts = bridge.contacts()
        val convByPeer = bridge.conversations().associateBy { it.peerIpk.toList() }
        val unread = bridge.unreadCounts().associate { it.peerIpk.toList() to it.count.toInt() }
        contacts.map { c ->
            val last = convByPeer[c.ipk.toList()]
            ChatSummary(
                peerHex = c.ipk.toHex(),
                name = c.name,
                lastPreview = last?.content,
                timestampMs = (last?.timestamp ?: c.addedAt).toLong() * 1000,
                status = c.status.toInt(),
                rejectReason = c.rejectReason?.toInt(),
                unreadCount = unread[c.ipk.toList()] ?: 0,
                lastOutgoing = last?.outgoing == true,
                lastDeleted = last?.deleted == true,
                lastStatus = last?.status?.toInt() ?: 1,
            )
        }.sortedByDescending { it.timestampMs }
    } catch (e: Exception) {
        Timber.tag(TAG).e(e, "Failed to load chats")
        emptyList()
    }
}
