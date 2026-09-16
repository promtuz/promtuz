package com.promtuz.chat.update

import android.content.Context
import androidx.work.BackoffPolicy
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import com.google.firebase.messaging.FirebaseMessaging
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.tasks.await
import kotlinx.coroutines.withTimeout
import org.koin.core.component.KoinComponent
import org.koin.core.component.inject
import timber.log.Timber

class UpdateWorker(context: Context, params: WorkerParameters) :
    CoroutineWorker(context, params), KoinComponent {
    private val updates: UpdateRepository by inject()

    override suspend fun doWork(): Result {
        val checked = attempt { updates.checkAndNotify() }
        // Topic setup is independent: devices without FCM can still check HTTPS.
        val subscribed = attempt {
            subscriptionLock.withLock {
                withTimeout(30_000) {
                    val selected = updates.channel
                    val messaging = FirebaseMessaging.getInstance()
                    messaging.subscribeToTopic("promtuz-updates-$selected").await()
                    check(selected == updates.channel) { "Update channel changed during subscription" }
                    for (channel in UpdateRepository.CHANNELS - selected) {
                        messaging.unsubscribeFromTopic("promtuz-updates-$channel").await()
                    }
                    check(selected == updates.channel) { "Update channel changed during subscription" }
                }
            }
        }
        return if (checked && subscribed) Result.success() else Result.retry()
    }

    private suspend fun attempt(block: suspend () -> Unit): Boolean = try {
        block()
        true
    } catch (error: TimeoutCancellationException) {
        Timber.tag("AppUpdater").w(error, "Update subscription timed out")
        false
    } catch (error: CancellationException) {
        throw error
    } catch (error: Exception) {
        Timber.tag("AppUpdater").w(error, "Background update check will retry")
        false
    }

    companion object {
        // Periodic checks and push-triggered checks can overlap.
        private val subscriptionLock = Mutex()
        private const val WORK_NAME = "app-update-check"
        private val constraints get() = Constraints.Builder()
            .setRequiredNetworkType(NetworkType.CONNECTED).build()

        fun schedule(context: Context) {
            val periodic = PeriodicWorkRequestBuilder<UpdateWorker>(24, TimeUnit.HOURS)
                .setInitialDelay(24, TimeUnit.HOURS)
                .setConstraints(constraints)
                .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 15, TimeUnit.MINUTES)
                .build()
            WorkManager.getInstance(context).enqueueUniquePeriodicWork(
                "$WORK_NAME-periodic", ExistingPeriodicWorkPolicy.KEEP, periodic,
            )
            enqueue(context)
        }

        fun enqueue(context: Context, replace: Boolean = false) {
            val request = OneTimeWorkRequestBuilder<UpdateWorker>()
                .setConstraints(constraints)
                .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 15, TimeUnit.MINUTES)
                .build()
            WorkManager.getInstance(context).enqueueUniqueWork(
                WORK_NAME,
                if (replace) ExistingWorkPolicy.REPLACE else ExistingWorkPolicy.KEEP,
                request,
            )
        }
    }
}
