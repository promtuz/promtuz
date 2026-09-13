package com.promtuz.chat.presentation.viewmodel

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/** Current editing activity, independent of whether a signal could reach its recipient. */
internal class OutgoingTyping(
    private val scope: CoroutineScope,
    private val now: () -> Long,
    private val idleMs: Long = 6000,
    private val refreshMs: Long = 4000,
    private val send: suspend (Boolean) -> Unit,
) {
    private var foreground = false
    private var activeUntil = 0L
    private var lastSent: Long? = null
    private var expiry: Job? = null
    private val signals = Channel<Boolean>(Channel.CONFLATED)

    init {
        scope.launch {
            for (active in signals) {
                try { send(active) }
                catch (e: CancellationException) { throw e }
                catch (_: Exception) { /* Ephemeral. A later edit or reconnect refreshes it. */ }
            }
        }
    }

    fun setForeground(value: Boolean) {
        foreground = value
        if (!value) stop()
    }

    fun edited(text: String) {
        if (!foreground) return
        if (text.isEmpty()) { stop(); return }
        activeUntil = now() + idleMs
        expiry?.cancel()
        expiry = scope.launch { delay(idleMs); stop() }
        if (lastSent == null || now() - lastSent!! >= refreshMs) refresh()
    }

    /** Peer online/foreground or our relay reconnected. Never revive an idle draft. */
    fun refresh() {
        if (!foreground || now() >= activeUntil) return
        lastSent = now()
        signals.trySend(true)
    }

    private fun stop() {
        expiry?.cancel()
        expiry = null
        activeUntil = 0
        if (lastSent != null) signals.trySend(false)
        lastSent = null
    }
}
