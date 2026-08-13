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

    /** Master switch for new-message notifications. Default on. */
    var notifEnabled: Boolean
        get() = prefs.getBoolean(NOTIF_ENABLED, true)
        set(value) = prefs.edit { putBoolean(NOTIF_ENABLED, value) }

    /** Show sender + text in the shade, vs a generic "New message". Default on. */
    var notifPreview: Boolean
        get() = prefs.getBoolean(NOTIF_PREVIEW, true)
        set(value) = prefs.edit { putBoolean(NOTIF_PREVIEW, value) }

    /** How new-message notifications alert. Default: buzz on every message. */
    var notifBuzz: NotifBuzz
        get() = runCatching { NotifBuzz.valueOf(prefs.getString(NOTIF_BUZZ, "")!!) }.getOrDefault(NotifBuzz.EveryMessage)
        set(value) = prefs.edit { putString(NOTIF_BUZZ, value.name) }

    /** In-app update channel override ("debug"/"release"); null = follow the installed build type. */
    var updateChannel: String?
        get() = prefs.getString(UPDATE_CHANNEL, null)
        set(value) = prefs.edit { putString(UPDATE_CHANNEL, value) }

    fun init(context: Context) {
        prefs = context.getSharedPreferences("chat_flags", Context.MODE_PRIVATE)
        _pinned.value = prefs.getStringSet(PINNED, emptySet()).orEmpty().toSet()
        _muted.value = prefs.getStringSet(MUTED, emptySet()).orEmpty().toSet()
    }

    fun togglePin(convHex: String) { _pinned.value = _pinned.value.toggled(convHex); persist() }
    fun toggleMute(convHex: String) { _muted.value = _muted.value.toggled(convHex); persist() }

    /**
     * Timestamp (unix seconds) of the newest message this chat has already
     * alerted for. Persisted rather than held in memory because the case that
     * needs it is a wake-drain in a fresh process, which starts with an empty
     * heap and an unread set the user was notified about hours ago.
     */
    fun lastAlerted(convHex: String): Long = prefs.getLong(ALERTED + convHex, 0L)

    fun setLastAlerted(convHex: String, tsSecs: Long) =
        prefs.edit { putLong(ALERTED + convHex, tsSecs) }

    /** Drop all flags for a forgotten contact. */
    fun forget(convHex: String) {
        _pinned.value = _pinned.value - convHex
        _muted.value = _muted.value - convHex
        prefs.edit { remove(ALERTED + convHex) }
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
    private const val NOTIF_ENABLED = "notif_enabled"
    private const val NOTIF_PREVIEW = "notif_preview"
    private const val NOTIF_BUZZ = "notif_buzz"
    private const val UPDATE_CHANNEL = "update_channel"
    private const val ALERTED = "alerted_"
}

/** New-message alert cadence, persisted via [ChatPrefs.notifBuzz]. */
enum class NotifBuzz { EveryMessage, Throttled, FirstOnly }
