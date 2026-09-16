package com.promtuz.core.push

import androidx.work.ExistingWorkPolicy
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.OutOfQuotaPolicy
import androidx.work.WorkManager
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage

/**
 * FCM entry point. Wakes are contentless "drain now" data messages — the actual
 * MLS decrypt happens inside libcore during the drain, never here. [onNewToken]
 * hands the token to libcore, which registers `P → token` with a gateway.
 */
class PushService : FirebaseMessagingService() {
    override fun onNewToken(token: String) {
        PushRegistrationWorker.enqueue(applicationContext, tokenChanged = true)
    }

    override fun onMessageReceived(message: RemoteMessage) {
        enqueueDrain()
    }

    override fun onDeletedMessages() {
        enqueueDrain()
    }

    private fun enqueueDrain() {
        val work = OneTimeWorkRequestBuilder<DrainWorker>()
            .setExpedited(OutOfQuotaPolicy.RUN_AS_NON_EXPEDITED_WORK_REQUEST)
            .build()
        // One worker owns the drain, but a later wake must renew its deadline.
        // KEEP can leave a just-arrived queued message waiting behind a worker
        // that is about to time out.
        WorkManager.getInstance(applicationContext)
            .enqueueUniqueWork("push-drain", ExistingWorkPolicy.REPLACE, work)
    }
}
