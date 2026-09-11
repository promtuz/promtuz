package com.promtuz.chat.presentation.viewmodel

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

/** Chat-local activity. Each member owns an independent expiry, refreshed by their signals. */
internal class TypingActivity(private val scope: CoroutineScope, private val ttlMs: Long) {
    private val expiries = mutableMapOf<String, Job>()
    private val _members = MutableStateFlow<Set<String>>(emptySet())
    val members: StateFlow<Set<String>> = _members.asStateFlow()
    private val _typing = MutableStateFlow(false)
    val typing: StateFlow<Boolean> = _typing.asStateFlow()

    // Called on the view model's main dispatcher, as are the expiry jobs.
    fun update(peer: String, active: Boolean) {
        expiries.remove(peer)?.cancel()
        if (active) {
            setMembers(_members.value + peer)
            expiries[peer] = scope.launch {
                delay(ttlMs)
                expiries.remove(peer)
                setMembers(_members.value - peer)
            }
        } else {
            setMembers(_members.value - peer)
        }
    }

    fun clear() {
        expiries.values.forEach { it.cancel() }
        expiries.clear()
        setMembers(emptySet())
    }

    private fun setMembers(value: Set<String>) {
        _members.value = value
        _typing.value = value.isNotEmpty()
    }
}
