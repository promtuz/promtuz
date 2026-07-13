package com.promtuz.core.push

import android.content.Context
import androidx.core.app.NotificationCompat
import androidx.work.CoroutineWorker
import androidx.work.ForegroundInfo
import androidx.work.WorkerParameters
import com.promtuz.chat.R
import com.promtuz.core.CoreBridge
import com.promtuz.core.adapter.CoreEventBus
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withTimeoutOrNull

/**
 * Keeps the process alive across an FCM wake while libcore connects + drains the
 * offline queue. [CoreBridge.onForeground] nudges the reconnect; we then wait
 * (bounded) for the first message write, by which point PushNotifier has posted,
 * before letting the process go.
 */
class DrainWorker(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {
    override suspend fun doWork(): Result {
        CoreBridge.onForeground()
        withTimeoutOrNull(DRAIN_WAIT_MS) {
            CoreEventBus.dbChanged.first { "messages" in it }
        }
        return Result.success()
    }

    // API < 31 runs expedited work as a foreground service and requires this. A minimal, low-key
    // notice on the sync channel; on 31+ it is never shown.
    override suspend fun getForegroundInfo(): ForegroundInfo {
        Notifications.ensureChannels(applicationContext)
        val notif = NotificationCompat.Builder(applicationContext, Notifications.SYNC_CHANNEL)
            .setSmallIcon(R.drawable.i_notifications)
            .setContentTitle("Checking for new messages")
            .setPriority(NotificationCompat.PRIORITY_MIN)
            .setOngoing(true)
            .build()
        return ForegroundInfo(SYNC_NOTIF_ID, notif)
    }

    private companion object {
        const val DRAIN_WAIT_MS = 12_000L
        const val SYNC_NOTIF_ID = 42
    }
}
