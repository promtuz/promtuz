package com.promtuz.core.push

import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.OutOfQuotaPolicy
import androidx.work.WorkManager
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage
import com.promtuz.core.CoreBridge

/**
 * FCM entry point. Wakes are contentless "drain now" data messages — the actual
 * MLS decrypt happens inside libcore during the drain, never here. [onNewToken]
 * hands the token to libcore, which registers `P → token` with a gateway.
 */
class PushService : FirebaseMessagingService() {
    override fun onNewToken(token: String) {
        CoreBridge.registerPushToken(token.toByteArray())
    }

    override fun onMessageReceived(message: RemoteMessage) {
        // The process may be cold — but Application.onCreate already ran and started libcore, so we
        // just need to nudge the drain and keep the process alive while it runs. Do that in an
        // expedited job (the connect + drain is real I/O); the drain delivers messages, and
        // PushNotifier (a long-lived observer) posts the notifications.
        val work = OneTimeWorkRequestBuilder<DrainWorker>()
            .setExpedited(OutOfQuotaPolicy.RUN_AS_NON_EXPEDITED_WORK_REQUEST)
            .build()
        WorkManager.getInstance(applicationContext).enqueue(work)
    }
}
