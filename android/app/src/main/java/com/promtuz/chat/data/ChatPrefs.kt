package com.promtuz.chat.data

import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking

/**
 * App settings, stored by libcore rather than by the platform.
 *
 * They were SharedPreferences, which `backup_rules.xml` does not ship — it
 * carries the encrypted blob and nothing else — so every reinstall silently
 * reset them. In core they ride the blob, and iOS gets them for free.
 *
 * Per-conversation flags (pin, mute) are not here at all any more: they are
 * facts about a conversation, so they live on it and arrive with the row.
 */
object ChatPrefs {
    private val scope = CoroutineScope(Dispatchers.IO)

    /** One-shot: has the notification-permission priming prompt been shown? */
    var notifPrimed: Boolean
        get() = bool(NOTIF_PRIMED, false)
        set(v) = put(NOTIF_PRIMED, v.toString())

    /** Master switch for new-message notifications. Default on. */
    var notifEnabled: Boolean
        get() = bool(NOTIF_ENABLED, true)
        set(v) = put(NOTIF_ENABLED, v.toString())

    /** Show sender + text in the shade, vs a generic "New message". Default on. */
    var notifPreview: Boolean
        get() = bool(NOTIF_PREVIEW, true)
        set(v) = put(NOTIF_PREVIEW, v.toString())

    /** How new-message notifications alert. Default: buzz on every message. */
    var notifBuzz: NotifBuzz
        get() = runCatching { NotifBuzz.valueOf(get(NOTIF_BUZZ)!!) }
            .getOrDefault(NotifBuzz.EveryMessage)
        set(v) = put(NOTIF_BUZZ, v.name)

    /** Update channel override ("debug"/"release"); null = follow the installed build. */
    var updateChannel: String?
        get() = get(UPDATE_CHANNEL)
        set(v) = put(UPDATE_CHANNEL, v.orEmpty())

    fun togglePin(convHex: String, pinned: Boolean) = scope.launch {
        runCatching { CoreBridge.setConversationPinned(convHex.fromHex(), pinned) }
    }

    fun toggleMute(convHex: String, muted: Boolean) = scope.launch {
        runCatching { CoreBridge.setConversationMuted(convHex.fromHex(), muted) }
    }

    /**
     * Newest message this chat has already alerted for, unix seconds. Persisted
     * rather than held in memory because the case that needs it is a wake-drain
     * in a fresh process, whose heap is empty and whose unread set is hours old.
     */
    fun setLastAlerted(convHex: String, tsSecs: Long) = scope.launch {
        runCatching { CoreBridge.setAlertedAt(convHex.fromHex(), tsSecs.toULong()) }
    }

    // Settings are read from composition and from the notification path, both of
    // which want an answer now. The table is tiny and local; a suspend getter
    // would turn every read site into a coroutine for no gain.
    // ponytail: blocking reads, cache in a StateFlow if a settings screen ever stutters.
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
}

/** New-message alert cadence, persisted via [ChatPrefs.notifBuzz]. */
enum class NotifBuzz { EveryMessage, Throttled, FirstOnly }
