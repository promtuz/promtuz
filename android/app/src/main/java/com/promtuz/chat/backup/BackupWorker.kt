package com.promtuz.chat.backup

import android.app.backup.BackupManager
import android.content.Context
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.NetworkType
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import com.promtuz.chat.security.RecoveryStore
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import timber.log.Timber
import java.io.File
import java.util.concurrent.TimeUnit

/**
 * Daily encrypted-backup snapshot (IDENTITY_RECOVERY.md §4). Writes the blob
 * to `files/recovery/backup.pzbk`; Android Auto Backup ships that one file
 * to the user's Drive app data (E2E when the device has a lock screen — see
 * data_extraction_rules.xml) and restores it before first launch. No Drive
 * API, no OAuth — the OS is the transport.
 *
 * Debounce: `dbChanged` (the reactive doorbell) marks a dirty flag; a clean
 * day skips the export entirely.
 */
class BackupWorker(context: Context, params: WorkerParameters) :
    CoroutineWorker(context, params) {

    override suspend fun doWork(): Result {
        if (!CoreBridge.shouldLaunchApp()) return Result.success() // pre-enrollment
        if (!prefs(applicationContext).getBoolean(KEY_DIRTY, false)) return Result.success()

        return try {
            val blob = CoreBridge.backupExport()
            val file = RecoveryStore.blobFile(applicationContext)
            file.parentFile?.mkdirs()
            // Atomic swap so Auto Backup never ships a half-written blob.
            val tmp = File(file.parentFile, "${file.name}.tmp")
            tmp.writeBytes(blob)
            if (!tmp.renameTo(file)) {
                tmp.delete()
                return Result.retry()
            }
            prefs(applicationContext).edit().putBoolean(KEY_DIRTY, false).apply()
            // Hint the OS that backup-worthy data changed (next backup window).
            BackupManager(applicationContext).dataChanged()
            if (blob.size > SIZE_WARN_BYTES) {
                Timber.tag("Backup").w("blob is ${blob.size / 1_000_000}MB — nearing quota")
            }
            Timber.tag("Backup").i("snapshot written (${blob.size} bytes)")
            Result.success()
        } catch (e: Exception) {
            Timber.tag("Backup").w(e, "snapshot failed")
            Result.retry()
        }
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

        /**
         * App-start hook: mark the dirty flag on every DB doorbell and keep
         * the daily snapshot scheduled (WiFi + charging, like the big apps).
         */
        fun start(context: Context) {
            val app = context.applicationContext
            scope.launch {
                CoreBridge.dbChanged.collect {
                    prefs(app).edit().putBoolean(KEY_DIRTY, true).apply()
                }
            }
            val request = PeriodicWorkRequestBuilder<BackupWorker>(24, TimeUnit.HOURS)
                .setConstraints(
                    Constraints.Builder()
                        .setRequiredNetworkType(NetworkType.UNMETERED)
                        .setRequiresCharging(true)
                        .build()
                )
                .build()
            WorkManager.getInstance(app).enqueueUniquePeriodicWork(
                WORK_NAME, ExistingPeriodicWorkPolicy.KEEP, request
            )
        }
    }
}
