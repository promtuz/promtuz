package com.promtuz.chat.data

import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking

/** Stored in core, so settings ride the backup blob. */
object ChatPrefs {
    private val scope = CoroutineScope(Dispatchers.IO)

    var notifPrimed: Boolean
        get() = bool(NOTIF_PRIMED, false)
        set(v) = put(NOTIF_PRIMED, v.toString())

    var notifEnabled: Boolean
        get() = bool(NOTIF_ENABLED, true)
        set(v) = put(NOTIF_ENABLED, v.toString())

    var notifPreview: Boolean
        get() = bool(NOTIF_PREVIEW, true)
        set(v) = put(NOTIF_PREVIEW, v.toString())

    var notifBuzz: NotifBuzz
        get() = runCatching { NotifBuzz.valueOf(get(NOTIF_BUZZ)!!) }
            .getOrDefault(NotifBuzz.EveryMessage)
        set(v) = put(NOTIF_BUZZ, v.name)

    var stickerColumns: Int
        get() = get(STICKER_COLUMNS)?.toIntOrNull()?.coerceIn(3, 6) ?: 5
        set(v) = put(STICKER_COLUMNS, v.toString())

    /** "debug" or "release"; null follows the installed build. */
    var updateChannel: String?
        get() = get(UPDATE_CHANNEL)
        set(v) = put(UPDATE_CHANNEL, v.orEmpty())

    fun togglePin(convHex: String, pinned: Boolean) = scope.launch {
        runCatching { CoreBridge.setConversationPinned(convHex.fromHex(), pinned) }
    }

    fun toggleMute(convHex: String, muted: Boolean) = scope.launch {
        runCatching { CoreBridge.setConversationMuted(convHex.fromHex(), muted) }
    }

    // Blocking reads, since composition and the notification path both want an answer now.
    private fun get(key: String): String? =
        runBlocking { runCatching { CoreBridge.pref(key) }.getOrNull() }?.takeIf { it.isNotEmpty() }

    private fun bool(key: String, default: Boolean) = get(key)?.toBooleanStrictOrNull() ?: default

    private fun put(key: String, value: String) {
        runBlocking { runCatching { CoreBridge.setPref(key, value) } }
    }

    private const val NOTIF_PRIMED = "notif_primed"
    private const val NOTIF_ENABLED = "notif_enabled"
    private const val NOTIF_PREVIEW = "notif_preview"
    private const val NOTIF_BUZZ = "notif_buzz"
    private const val UPDATE_CHANNEL = "update_channel"
    private const val STICKER_COLUMNS = "sticker_columns"
}

enum class NotifBuzz { EveryMessage, Throttled, FirstOnly }
