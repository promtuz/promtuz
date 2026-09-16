package com.promtuz.core.push

import android.content.Context
import androidx.work.BackoffPolicy
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import com.google.firebase.messaging.FirebaseMessaging
import com.promtuz.core.CoreBridge
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.tasks.await
import timber.log.Timber

class PushRegistrationWorker(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {
    override suspend fun doWork(): Result = try {
        val token = FirebaseMessaging.getInstance().token.await()
        CoreBridge.registerPushToken(token.toByteArray())
        Result.success()
    } catch (e: CancellationException) {
        throw e
    } catch (e: Exception) {
        Timber.tag("Push").w(e, "Push registration will retry")
        Result.retry()
    }

    companion object {
        fun enqueue(context: Context, tokenChanged: Boolean = false) {
            val work = OneTimeWorkRequestBuilder<PushRegistrationWorker>()
                .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
                .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 10, TimeUnit.SECONDS)
                .build()
            WorkManager.getInstance(context)
                .enqueueUniqueWork(
                    "push-registration",
                    if (tokenChanged) ExistingWorkPolicy.REPLACE else ExistingWorkPolicy.KEEP,
                    work,
                )
        }
    }
}
