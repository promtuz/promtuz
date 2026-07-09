package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.domain.model.Activity
import com.promtuz.chat.domain.model.MessageContent
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

    private val _typing = MutableStateFlow(false)
    val typing: StateFlow<Boolean> = _typing.asStateFlow()

    fun init(peerIpk: ByteArray) {
        if (started) return
        started = true
        peer = peerIpk

        viewModelScope.launch {
            observeQuery(setOf("messages", "reactions")) { load() }.collect { _messages.value = it }
        }

        var expiry: Job? = null
        viewModelScope.launch {
            CoreBridge.activity.filter { it.peer.contentEquals(peer) }.collect { sig ->
                if (Activity.Typing in Activity.fromBits(sig.bits)) {
                    _typing.value = true
                    expiry?.cancel()
                    expiry = launch { delay(TYPING_TTL_MS); _typing.value = false }
                } else {
                    expiry?.cancel()
                    _typing.value = false
                }
            }
        }
    }

    private suspend fun load(): List<UiMessage> {
        val rows = CoreBridge.messages(peer, 200)                    // oldest-first
        val byMsg = CoreBridge.reactions(peer).groupBy { it.dispatchId.toHex() }
        // reversed → newest at index 0 → drawn at the bottom under reverseLayout
        return rows.asReversed().map { it.toUi(byMsg) }
    }

    fun send() {
        val text = input.value.trim()
        if (text.isEmpty()) return
        input.value = ""
        fire { CoreBridge.sendMessage(peer, text) }
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
    }
}

private fun MessageRecord.toUi(reactionsByMsg: Map<String, List<ReactionRecord>>): UiMessage {
    val didHex = dispatchId?.toHex()
    val reactions = didHex?.let { reactionsByMsg[it] }
        ?.groupBy { it.emoji }
        ?.map { (emoji, rs) -> ReactionGroup(emoji, rs.size, rs.any { it.mine }) }
        ?: emptyList()
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
    )
}
