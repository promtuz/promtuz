package com.promtuz.core.push

import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.media.AudioAttributes
import android.media.RingtoneManager
import android.net.Uri
import android.provider.Settings
import androidx.core.content.edit
import androidx.core.net.toUri

enum class AlertKind(val key: String, val label: String) { Chats("chats", "Chats"), Groups("groups", "Groups") }

enum class Vibe(val label: String, val pattern: LongArray?) {
    Default("Default", null),
    Short("Short", longArrayOf(0, 120)),
    Long("Long", longArrayOf(0, 400, 150, 400)),
    Off("Off", null),
}

/** [sound] is empty for Android's default, "none" for silence, or a ringtone URI. */
data class AlertStyle(val sound: String = "", val vibe: Vibe = Vibe.Default, val popup: Boolean = true) {
    val soundUri: Uri? get() = when (sound) {
        "" -> Settings.System.DEFAULT_NOTIFICATION_URI
        "none" -> null
        else -> sound.toUri()
    }

    fun soundLabel(ctx: Context): String = when (sound) {
        "" -> "Default"
        "none" -> "None"
        else -> runCatching { RingtoneManager.getRingtone(ctx, sound.toUri())?.getTitle(ctx) }.getOrNull() ?: "Custom"
    }
}

object Notifications {
    const val REQUESTS_CHANNEL = "message_requests"
    const val IN_APP_CHANNEL = "in_app"
    const val SYNC_CHANNEL = "sync"
    const val UPDATES_CHANNEL = "app_updates"
    const val GROUP_KEY = "com.promtuz.chat.MESSAGES"
    private const val LEGACY_MESSAGES = "messages"

    // Sound URIs only mean something on this phone, so these stay out of core's backed-up prefs.
    private fun prefs(ctx: Context) = ctx.getSharedPreferences("notification_channels", Context.MODE_PRIVATE)

    /** Android freezes a channel's sound once created, so each change gets a fresh channel id. */
    fun channel(ctx: Context, kind: AlertKind) = "${kind.key}_${prefs(ctx).getInt("${kind.key}_rev", 0)}"

    fun style(ctx: Context, kind: AlertKind): AlertStyle = prefs(ctx).let { p ->
        AlertStyle(
            p.getString("${kind.key}_sound", "")!!,
            runCatching { Vibe.valueOf(p.getString("${kind.key}_vibe", null)!!) }.getOrDefault(Vibe.Default),
            p.getBoolean("${kind.key}_popup", true),
        )
    }

    fun setStyle(ctx: Context, kind: AlertKind, style: AlertStyle) {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        nm.deleteNotificationChannel(channel(ctx, kind))
        val p = prefs(ctx)
        p.edit {
            putInt("${kind.key}_rev", p.getInt("${kind.key}_rev", 0) + 1)
            putString("${kind.key}_sound", style.sound)
            putString("${kind.key}_vibe", style.vibe.name)
            putBoolean("${kind.key}_popup", style.popup)
        }
        createKind(ctx, nm, kind)
    }

    fun reset(ctx: Context) = AlertKind.entries.forEach { setStyle(ctx, it, AlertStyle()) }

    fun ensureChannels(ctx: Context) {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        // The single "messages" channel split in two; the new ones keep what the user set on it.
        val legacy = nm.getNotificationChannel(LEGACY_MESSAGES)
        AlertKind.entries.forEach {
            val fresh = legacy != null && nm.getNotificationChannel(channel(ctx, it)) == null
            if (fresh) adopt(ctx, it, legacy!!)
            createKind(ctx, nm, it, blocked = fresh && legacy!!.importance == NotificationManager.IMPORTANCE_NONE)
        }
        if (legacy != null) nm.deleteNotificationChannel(LEGACY_MESSAGES)
        // Banners while the app is open; Promtuz plays its own sound for those, per the in-app settings.
        nm.createNotificationChannel(
            NotificationChannel(IN_APP_CHANNEL, "While using Promtuz", NotificationManager.IMPORTANCE_HIGH).apply {
                setSound(null, null)
                enableVibration(false)
            }
        )
        // No heads-up for strangers by default; the user can raise it in Android's settings.
        nm.createNotificationChannel(
            NotificationChannel(REQUESTS_CHANNEL, "Message requests", NotificationManager.IMPORTANCE_DEFAULT)
        )
        nm.createNotificationChannel(
            NotificationChannel(SYNC_CHANNEL, "Syncing", NotificationManager.IMPORTANCE_MIN)
        )
        nm.createNotificationChannel(
            NotificationChannel(UPDATES_CHANNEL, "App updates", NotificationManager.IMPORTANCE_LOW).apply {
                setSound(null, null)
                enableVibration(false)
            }
        )
    }

    private fun adopt(ctx: Context, kind: AlertKind, legacy: NotificationChannel) {
        prefs(ctx).edit {
            putString("${kind.key}_sound", when (legacy.sound) {
                null -> "none"
                Settings.System.DEFAULT_NOTIFICATION_URI -> ""
                else -> legacy.sound.toString()
            })
            putBoolean("${kind.key}_popup", legacy.importance >= NotificationManager.IMPORTANCE_HIGH)
        }
    }

    private fun createKind(ctx: Context, nm: NotificationManager, kind: AlertKind, blocked: Boolean = false) {
        val s = style(ctx, kind)
        val importance = when {
            blocked -> NotificationManager.IMPORTANCE_NONE
            s.popup -> NotificationManager.IMPORTANCE_HIGH
            else -> NotificationManager.IMPORTANCE_DEFAULT
        }
        nm.createNotificationChannel(NotificationChannel(channel(ctx, kind), kind.label, importance).apply {
            if (s.sound.isNotEmpty()) setSound(s.soundUri, AudioAttributes.Builder()
                .setUsage(AudioAttributes.USAGE_NOTIFICATION_COMMUNICATION_INSTANT)
                .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION).build())
            when (s.vibe) {
                Vibe.Default -> {}
                Vibe.Off -> enableVibration(false)
                else -> { enableVibration(true); vibrationPattern = s.vibe.pattern }
            }
        })
    }

    /** Whether Android lets message alerts through at all. */
    fun messagesBlocked(ctx: Context): Boolean {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        return AlertKind.entries.all { nm.getNotificationChannel(channel(ctx, it))?.importance == NotificationManager.IMPORTANCE_NONE }
    }
}
