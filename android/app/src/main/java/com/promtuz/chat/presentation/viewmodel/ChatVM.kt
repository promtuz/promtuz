package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.os.SystemClock
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.domain.model.Activity
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.domain.model.Quote
import com.promtuz.chat.presentation.state.ConnectionState
import com.promtuz.chat.domain.model.ReactionGroup
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.launch
import uniffi.core.MessageRecord
import uniffi.core.ReactionRecord

/**
 * Reactive chat. [messages] observes the DB — re-read on every commit touching
 * messages/reactions — so send / receive / edit / delete / reaction / receipt all
 * surface as row updates with no hand-patching. [input] is the draft, cleared the
 * instant [send] fires (so the editor empties immediately). Newest message sits at
 * index 0 and the list draws reversed, so new messages land at the bottom. Typing
 * is an ephemeral signal, timed out client-side.
 */
class ChatVM(private val application: Application) : ViewModel() {
    private var peer: ByteArray = ByteArray(32)
    private var started = false

    private val _messages = MutableStateFlow<List<UiMessage>>(emptyList())
    val messages: StateFlow<List<UiMessage>> = _messages.asStateFlow()

    /** Composer draft; two-way bound to the input field, cleared on [send]. */
    val input = MutableStateFlow("")

    /** Reply/edit staging shown as a chip above the composer; consumed by [send]. */
    val composerAction = MutableStateFlow<ComposerAction?>(null)

    private val _typing = MutableStateFlow(false)
    val typing: StateFlow<Boolean> = _typing.asStateFlow()
    private var typingExpiry: Job? = null

    private val _presence = MutableStateFlow<Presence?>(null)
    val presence: StateFlow<Presence?> = _presence.asStateFlow()

    fun init(peerIpk: ByteArray) {
        if (started) return
        started = true
        peer = peerIpk

        var newestIncoming: String? = null
        viewModelScope.launch {
            observeQuery(setOf("messages", "reactions")) { load() }.collect { list ->
                _messages.value = list
                // Their message just landed — the typing bubble hands off to it,
                // and with this chat on screen it's read: receipt the high-water mark.
                val newest = list.firstOrNull { !it.outgoing }
                if (newest?.key != newestIncoming) {
                    newestIncoming = newest?.key
                    clearTyping()
                    newest?.dispatchIdHex?.let { did ->
                        fire { CoreBridge.markRead(peer, did.fromHex()) }
                    }
                }
            }
        }

        viewModelScope.launch {
            CoreBridge.activity.filter { it.peer.contentEquals(peer) }.collect { sig ->
                if (Activity.Typing in Activity.fromBits(sig.bits)) {
                    _typing.value = true
                    typingExpiry?.cancel()
                    typingExpiry = viewModelScope.launch { delay(TYPING_TTL_MS); _typing.value = false }
                } else clearTyping()
            }
        }

        viewModelScope.launch {
            CoreBridge.presence.filter { it.peer.contentEquals(peer) }.collect { sig ->
                _presence.value = sig.presence
            }
        }

        // The relay drops a subscribe sent while disconnected, so re-express
        // interest on every (re)connect, not once at chat-open.
        viewModelScope.launch {
            CoreBridge.connection.filter { it == ConnectionState.Connected }.collect {
                runCatching { CoreBridge.subscribePresence(listOf(peer)) }
            }
        }

        // Outbound typing: refresh under the peer's TTL while keystrokes flow,
        // one idle signal when the draft empties (send() clears input → same path).
        var lastSentAt = 0L
        viewModelScope.launch {
            input.collect { text ->
                if (text.isEmpty()) {
                    if (lastSentAt != 0L) {
                        lastSentAt = 0L
                        runCatching { CoreBridge.setActivity(peer, 0) }
                    }
                } else {
                    val now = SystemClock.uptimeMillis()
                    if (now - lastSentAt >= TYPING_RESEND_MS) {
                        lastSentAt = now
                        runCatching { CoreBridge.setActivity(peer, Activity.Typing.bit) }
                    }
                }
            }
        }
    }

    private fun clearTyping() {
        typingExpiry?.cancel()
        _typing.value = false
    }

    private suspend fun load(): List<UiMessage> {
        val rows = CoreBridge.messages(peer, 200)                    // oldest-first
        val byMsg = CoreBridge.reactions(peer).groupBy { it.dispatchId.toHex() }
        // Quote resolution: replies name a dispatch_id; snippet comes from the
        // loaded window (null text → "unavailable" shell, e.g. outside window).
        val byDid = rows.asSequence().mapNotNull { r -> r.dispatchId?.let { it.toHex() to r } }.toMap()
        // reversed → newest at index 0 → drawn at the bottom under reverseLayout
        return rows.asReversed().map { it.toUi(byMsg, byDid) }
    }

    fun send() {
        val text = input.value.trim()
        if (text.isEmpty()) return
        val action = composerAction.value
        input.value = ""
        composerAction.value = null
        when (action) {
            is ComposerAction.Edit -> action.msg.dispatchIdHex?.let { edit(it, text) }
            is ComposerAction.Reply -> fire {
                CoreBridge.sendMessage(peer, text, action.msg.dispatchIdHex?.fromHex())
            }
            null -> fire { CoreBridge.sendMessage(peer, text) }
        }
    }

    fun beginReply(msg: UiMessage) {
        composerAction.value = ComposerAction.Reply(msg)
    }

    fun beginEdit(msg: UiMessage) {
        composerAction.value = ComposerAction.Edit(msg)
        input.value = (msg.content as? MessageContent.Text)?.text.orEmpty()
    }

    fun cancelComposerAction() {
        if (composerAction.value is ComposerAction.Edit) input.value = ""
        composerAction.value = null
    }

    /** Tap on a quick-reaction or an existing chip: mine → remove, else add. */
    fun toggleReaction(msg: UiMessage, emoji: String) {
        val id = msg.dispatchIdHex ?: return
        val mine = msg.reactions.any { it.emoji == emoji && it.mine }
        react(id, emoji, add = !mine)
    }

    fun edit(dispatchIdHex: String, text: String) =
        fire { CoreBridge.editMessage(peer, dispatchIdHex.fromHex(), text) }

    fun delete(dispatchIdHex: String, forEveryone: Boolean) =
        fire { CoreBridge.deleteMessage(peer, dispatchIdHex.fromHex(), forEveryone) }

    fun react(dispatchIdHex: String, emoji: String, add: Boolean) =
        fire { CoreBridge.react(peer, dispatchIdHex.fromHex(), emoji, add) }

    private fun fire(block: suspend () -> Unit) = viewModelScope.launch { runCatching { block() } }

    private companion object {
        const val TYPING_TTL_MS = 6_000L

        /** Outbound refresh cadence; must stay under the peer's [TYPING_TTL_MS]. */
        const val TYPING_RESEND_MS = 4_000L
    }
}

/** What the next [ChatVM.send] means: a staged reply or an in-place edit. */
sealed interface ComposerAction {
    val msg: UiMessage

    data class Reply(override val msg: UiMessage) : ComposerAction
    data class Edit(override val msg: UiMessage) : ComposerAction
}

private fun MessageRecord.toUi(
    reactionsByMsg: Map<String, List<ReactionRecord>>,
    byDid: Map<String, MessageRecord>,
): UiMessage {
    val didHex = dispatchId?.toHex()
    val reactions = didHex?.let { reactionsByMsg[it] }
        ?.groupBy { it.emoji }
        ?.map { (emoji, rs) -> ReactionGroup(emoji, rs.size, rs.any { it.mine }) }
        ?: emptyList()
    val quote = replyTo?.toHex()?.let { rtHex ->
        val quoted = byDid[rtHex]
        Quote(
            dispatchIdHex = rtHex,
            text = quoted?.takeIf { !it.deleted }?.content,
            outgoing = quoted?.outgoing ?: false,
        )
    }
    return UiMessage(
        key = didHex ?: id,
        localId = id,
        dispatchIdHex = didHex,
        content = MessageContent.Text(content),
        outgoing = outgoing,
        status = SendStatus.from(status.toInt()),
        edited = edited,
        deleted = deleted,
        timestampMs = timestamp.toLong() * 1000,
        reactions = reactions,
        quote = quote,
    )
}
