package com.promtuz.chat.presentation.viewmodel

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

/** One app-wide activity snapshot. Opening a screen never restarts a peer's expiry. */
internal class ConversationActivity(private val scope: CoroutineScope, private val ttlMs: Long) {
    private val expiries = mutableMapOf<Pair<String, String>, Job>()
    private val _members = MutableStateFlow<Map<String, Map<String, Int>>>(emptyMap())
    val members = _members.asStateFlow()
    private val _byChat = MutableStateFlow<Map<String, Int>>(emptyMap())
    val byChat = _byChat.asStateFlow()

    // Updates and expiry jobs run on the app view model's main dispatcher.
    fun update(chat: String, peer: String, bits: Int) {
        val key = chat to peer
        expiries.remove(key)?.cancel()
        val people = _members.value[chat].orEmpty()
        val next = if (bits == 0) people - peer else people + (peer to bits)
        _members.value = if (next.isEmpty()) _members.value - chat else _members.value + (chat to next)
        _byChat.value = _members.value.mapValues { (_, values) -> values.values.fold(0) { a, b -> a or b } }
        if (bits != 0) expiries[key] = scope.launch {
            delay(ttlMs)
            update(chat, peer, 0)
        }
    }
}
