package com.promtuz.chat.presentation.viewmodel

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

/** Briefly retain the visual surface when a stop signal overtakes the message. */
internal class TypingPresentation(private val scope: CoroutineScope, private val graceMs: Long = 600) {
    private val pendingStops = mutableMapOf<String, Job>()
    private val _members = MutableStateFlow<Set<String>>(emptySet())
    val members = _members.asStateFlow()

    fun update(active: Set<String>) {
        active.forEach { pendingStops.remove(it)?.cancel() }
        val removed = _members.value - active
        _members.value = _members.value + active
        removed.forEach { peer ->
            if (peer !in pendingStops) pendingStops[peer] = scope.launch {
                delay(graceMs)
                pendingStops.remove(peer)
                _members.value = _members.value - peer
            }
        }
    }

    fun consume(peer: String) {
        pendingStops.remove(peer)?.cancel()
        _members.value = _members.value - peer
    }
}
