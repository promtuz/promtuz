package com.promtuz.core.push

import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context

object Notifications {
    const val MESSAGES_CHANNEL = "messages"
    const val REQUESTS_CHANNEL = "message_requests"
    const val SYNC_CHANNEL = "sync"
    const val UPDATES_CHANNEL = "app_updates"
    const val GROUP_KEY = "com.promtuz.chat.MESSAGES"

    fun ensureChannels(ctx: Context) {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(MESSAGES_CHANNEL, "Messages", NotificationManager.IMPORTANCE_HIGH)
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
}
