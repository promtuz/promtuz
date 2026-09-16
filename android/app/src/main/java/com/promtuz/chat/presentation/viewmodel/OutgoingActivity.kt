package com.promtuz.chat.presentation.viewmodel

import com.promtuz.chat.domain.model.Activity
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/** Prioritizes the visible sticker panel over typing, including expiry and reconnects. */
internal class OutgoingActivity(
    private val scope: CoroutineScope,
    private val now: () -> Long,
    private val idleMs: Long = 6000,
    private val refreshMs: Long = 4000,
    private val send: suspend (Int) -> Unit,
) {
    private var foreground = false
    private var activeUntil = 0L
    private var lastSent: Long? = null
    private var expiry: Job? = null
    private val signals = Channel<Int>(Channel.CONFLATED)
    private var choosingSticker = false
    private var choosingRefresh: Job? = null
    private var lastActivity = 0

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
        updateStickerRefresh()
    }

    fun setChoosingSticker(value: Boolean) {
        if (choosingSticker == value) return
        choosingSticker = value
        updateStickerRefresh()
        signal()
    }

    private fun updateStickerRefresh() {
        choosingRefresh?.cancel()
        choosingRefresh = if (foreground && choosingSticker) scope.launch {
            while (true) { refresh(); delay(refreshMs) }
        } else null
    }

    private fun signal(force: Boolean = false) {
        val activity = when {
            !foreground -> 0
            choosingSticker -> Activity.ChoosingSticker.bit
            now() < activeUntil -> Activity.Typing.bit
            else -> 0
        }
        if (force || activity != lastActivity) signals.trySend(activity)
        lastActivity = activity
    }

    fun edited(text: String) {
        if (!foreground) return
        if (text.isEmpty()) { stop(); return }
        activeUntil = now() + idleMs
        expiry?.cancel()
        expiry = scope.launch { delay(idleMs); stop() }
        if (lastSent == null || now() - lastSent!! >= refreshMs) refresh()
        else signal()
    }

    /** Peer online/foreground or our relay reconnected. Never revive an idle draft. */
    fun refresh() {
        if (!foreground || (!choosingSticker && now() >= activeUntil)) return
        lastSent = now()
        signal(force = true)
    }

    private fun stop() {
        expiry?.cancel()
        expiry = null
        activeUntil = 0
        signal()
        lastSent = null
    }
}
