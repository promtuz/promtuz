package com.promtuz.chat.data

import android.content.Context
import android.content.SharedPreferences
import androidx.core.content.edit
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow

/**
 * Local per-peer chat flags (pinned / muted) — UI preferences, not conversation
 * data, so they live in prefs rather than libcore. Surfaced as StateFlows so the
 * home list re-sorts / restyles the instant a flag toggles.
 */
object ChatPrefs {
    private lateinit var prefs: SharedPreferences

    private val _pinned = MutableStateFlow<Set<String>>(emptySet())
    val pinned: StateFlow<Set<String>> = _pinned

    private val _muted = MutableStateFlow<Set<String>>(emptySet())
    val muted: StateFlow<Set<String>> = _muted

    /** One-shot: has the notification-permission priming prompt been shown? */
    var notifPrimed: Boolean
        get() = prefs.getBoolean(NOTIF_PRIMED, false)
        set(value) = prefs.edit { putBoolean(NOTIF_PRIMED, value) }

    fun init(context: Context) {
        prefs = context.getSharedPreferences("chat_flags", Context.MODE_PRIVATE)
        _pinned.value = prefs.getStringSet(PINNED, emptySet()).orEmpty().toSet()
        _muted.value = prefs.getStringSet(MUTED, emptySet()).orEmpty().toSet()
    }

    fun togglePin(peerHex: String) { _pinned.value = _pinned.value.toggled(peerHex); persist() }
    fun toggleMute(peerHex: String) { _muted.value = _muted.value.toggled(peerHex); persist() }

    /** Drop all flags for a forgotten contact. */
    fun forget(peerHex: String) {
        _pinned.value = _pinned.value - peerHex
        _muted.value = _muted.value - peerHex
        persist()
    }

    private fun persist() = prefs.edit {
        putStringSet(PINNED, _pinned.value)
        putStringSet(MUTED, _muted.value)
    }

    private fun Set<String>.toggled(x: String) = if (x in this) this - x else this + x

    private const val PINNED = "pinned"
    private const val MUTED = "muted"
    private const val NOTIF_PRIMED = "notif_primed"
}
