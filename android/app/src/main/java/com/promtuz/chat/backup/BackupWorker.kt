package com.promtuz.chat.backup

import android.app.backup.BackupManager
import android.content.Context
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ExistingWorkPolicy
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import com.promtuz.chat.security.RecoveryStore
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import timber.log.Timber
import java.io.File
import java.util.concurrent.TimeUnit

/** Writes the encrypted blob that Auto Backup ships to Drive, but only after a DB change marked it dirty. */
class BackupWorker(context: Context, params: WorkerParameters) :
    CoroutineWorker(context, params) {

    override suspend fun doWork(): Result {
        if (!CoreBridge.shouldLaunchApp()) return Result.success() // pre-enrollment
        val prefs = prefs(applicationContext)
        if (!prefs.getBoolean(KEY_DIRTY, false)) return Result.success()
        // Cleared before the export: a change that lands meanwhile marks it again.
        prefs.edit().putBoolean(KEY_DIRTY, false).commit()

        return try {
            writing.withLock { snapshot() }
        } catch (e: Exception) {
            prefs.edit().putBoolean(KEY_DIRTY, true).apply()
            Timber.tag("Backup").w(e, "snapshot failed")
            Result.retry()
        }
    }

    private suspend fun snapshot(): Result {
        val file = RecoveryStore.blobFile(applicationContext)

        // Never replace a good blob with an empty one. A restore that lost the history leaves an empty DB
        // that looks just like an empty address book, and this blob may be the only copy.
        if (file.exists() && file.length() > 0 && CoreBridge.contacts().isEmpty()) {
            Timber.tag("Backup")
                .w("no contacts but a ${file.length()}-byte blob exists — refusing to overwrite it")
            return Result.success()
        }

        val blob = CoreBridge.backupExport()
        file.parentFile?.mkdirs()
        // Atomic swap so Auto Backup never ships a half-written blob.
        val tmp = File(file.parentFile, "${file.name}.tmp")
        tmp.writeBytes(blob)
        if (!tmp.renameTo(file)) {
            tmp.delete()
            prefs(applicationContext).edit().putBoolean(KEY_DIRTY, true).apply()
            return Result.retry()
        }
        BackupManager(applicationContext).dataChanged()
        if (blob.size > SIZE_WARN_BYTES) {
            Timber.tag("Backup").w("blob is ${blob.size / 1_000_000}MB, nearing quota")
        }
        Timber.tag("Backup").i("snapshot written (${blob.size} bytes)")
        return Result.success()
    }

    companion object {
        private const val WORK_NAME = "recovery-backup"
        private const val PREFS = "backup"
        private const val KEY_DIRTY = "dirty"

        /** Auto Backup quota is 25MB; warn well before it. */
        private const val SIZE_WARN_BYTES = 20_000_000

        private fun prefs(context: Context) =
            context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

        private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        /** The periodic and the one-shot job share the blob file. */
        private val writing = Mutex()

        /** Every trip to the background keeps the local blob fresh; the daily job is the safety net. */
        fun start(context: Context) {
            val app = context.applicationContext
            scope.launch {
                CoreBridge.dbChanged.collect {
                    prefs(app).edit().putBoolean(KEY_DIRTY, true).apply()
                }
            }
            ProcessLifecycleOwner.get().lifecycle.addObserver(object : DefaultLifecycleObserver {
                override fun onStop(owner: LifecycleOwner) = snapshotNow(app)
            })
            val request = PeriodicWorkRequestBuilder<BackupWorker>(24, TimeUnit.HOURS)
                .setConstraints(Constraints.Builder().setRequiresCharging(true).build())
                .build()
            WorkManager.getInstance(app).enqueueUniquePeriodicWork(
                WORK_NAME, ExistingPeriodicWorkPolicy.KEEP, request
            )
        }

        fun snapshotNow(context: Context) {
            WorkManager.getInstance(context.applicationContext).enqueueUniqueWork(
                "$WORK_NAME-now",
                ExistingWorkPolicy.KEEP,
                OneTimeWorkRequestBuilder<BackupWorker>().build(),
            )
        }
    }
}
