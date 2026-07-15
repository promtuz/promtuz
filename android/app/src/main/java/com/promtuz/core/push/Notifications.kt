package com.promtuz.core.push

import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context

/** Notification channels + the message group key, created once. minSdk 26, so channels always exist. */
object Notifications {
    const val MESSAGES_CHANNEL = "messages"
    const val SYNC_CHANNEL = "sync"
    const val GROUP_KEY = "com.promtuz.chat.MESSAGES"

    fun ensureChannels(ctx: Context) {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(MESSAGES_CHANNEL, "Messages", NotificationManager.IMPORTANCE_HIGH)
        )
        // Low-key channel for the brief foreground notice the drain worker shows on API < 31,
        // where expedited work runs as a foreground service.
        nm.createNotificationChannel(
            NotificationChannel(SYNC_CHANNEL, "Syncing", NotificationManager.IMPORTANCE_MIN)
        )
    }
}
