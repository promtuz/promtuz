package com.promtuz.core.push

import androidx.work.ExistingWorkPolicy
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.OutOfQuotaPolicy
import androidx.work.WorkManager
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage
import com.promtuz.chat.update.UpdateWorker

/**
 * Routes contentless message wakes and public release hints to separate jobs.
 * Both jobs fetch and verify their content before showing a notification.
 */
class PushService : FirebaseMessagingService() {
    override fun onNewToken(token: String) {
        PushRegistrationWorker.enqueue(applicationContext, tokenChanged = true)
        UpdateWorker.enqueue(applicationContext, replace = true)
    }

    override fun onMessageReceived(message: RemoteMessage) {
        when (message.data["type"]) {
            "app_update" -> UpdateWorker.enqueue(applicationContext, replace = true)
            null -> enqueueDrain()
        }
    }

    override fun onDeletedMessages() {
        enqueueDrain()
        UpdateWorker.enqueue(applicationContext)
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
