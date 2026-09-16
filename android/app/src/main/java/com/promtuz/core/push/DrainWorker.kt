package com.promtuz.core.push

import android.content.Context
import androidx.core.app.NotificationCompat
import androidx.work.CoroutineWorker
import androidx.work.ForegroundInfo
import androidx.work.WorkerParameters
import com.promtuz.chat.R
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CancellationException
import timber.log.Timber

/** Keeps the wake job alive through message sync and notification posting. */
class DrainWorker(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {
    override suspend fun doWork(): Result {
        if (!CoreBridge.shouldLaunchApp()) return Result.success()
        val synced = try {
            CoreBridge.syncMessages()
            true
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Timber.tag("Push").w(e, "Message sync will retry")
            false
        }
        // A partial drain can still have delivered messages.
        val notified = try {
            PushNotifier.refresh()
            true
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Timber.tag("Push").w(e, "Notification posting will retry")
            false
        }
        return if (synced && notified) Result.success() else Result.retry()
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
        const val SYNC_NOTIF_ID = 42
    }
}
