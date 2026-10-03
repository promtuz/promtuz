package com.promtuz.core.push

import androidx.work.ExistingWorkPolicy
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.OutOfQuotaPolicy
import androidx.work.WorkManager
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage
import com.promtuz.chat.update.UpdateWorker

/** Pushes carry no content; each job fetches and verifies its own before notifying. */
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
        // REPLACE renews the deadline; KEEP could leave a new message behind a worker about to time out.
        WorkManager.getInstance(applicationContext)
            .enqueueUniqueWork("push-drain", ExistingWorkPolicy.REPLACE, work)
    }
}
