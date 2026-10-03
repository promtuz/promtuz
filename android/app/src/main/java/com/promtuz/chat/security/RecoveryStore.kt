package com.promtuz.chat.security

import android.content.Context
import com.google.android.gms.auth.blockstore.Blockstore
import com.google.android.gms.auth.blockstore.RetrieveBytesRequest
import com.google.android.gms.auth.blockstore.StoreBytesData
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.tasks.await
import timber.log.Timber
import java.io.File

/** The identity and the history restore separately, so callers must report a history that did not come back. */
sealed interface BlobOutcome {
    data class Imported(val bytes: Long) : BlobOutcome

    data object Absent : BlobOutcome

    data class Failed(val reason: String) : BlobOutcome
}

/** Auto Backup restores the blob before first launch, so it is on disk when Block Store or the phrase
 *  brings the identity back. Without Google services both fail silently and only the phrase works. */
object RecoveryStore {
    private const val BS_KEY = "promtuz.isk"

    /** backupImport replaces it when a blob exists. */
    private const val PLACEHOLDER_NAME = "Restored"

    fun blobFile(context: Context) = File(context.filesDir, "recovery/backup.pzbk")

    /** Call after enroll and after any restore. */
    suspend fun escrow(context: Context) {
        try {
            val isk = CoreBridge.escrowSecret()
            val data = StoreBytesData.Builder()
                .setKey(BS_KEY)
                .setBytes(isk)
                .setShouldBackupToCloud(true)
                .build()
            Blockstore.getClient(context).storeBytes(data).await()
            Timber.tag("Recovery").i("isk escrowed to Block Store")
        } catch (e: Exception) {
            // No GMS, no lock screen or a transient failure; the phrase still covers the user.
            Timber.tag("Recovery").w(e, "Block Store escrow failed")
        }
    }

    suspend fun tryAutoRestore(context: Context): Boolean {
        val isk = try {
            val req = RetrieveBytesRequest.Builder().setKeys(listOf(BS_KEY)).build()
            Blockstore.getClient(context).retrieveBytes(req).await()
                .blockstoreDataMap[BS_KEY]?.bytes
        } catch (e: Exception) {
            Timber.tag("Recovery").i("Block Store lookup failed: ${e.message}")
            null
        } ?: return false

        return try {
            CoreBridge.adoptEscrowedSecret(isk, PLACEHOLDER_NAME)
            // Nothing on screen reports this path, so the log has to make a lost history obvious.
            when (val outcome = importBlobIfPresent(context)) {
                is BlobOutcome.Imported -> Timber.tag("Recovery")
                    .i("identity restored via Block Store; history imported (${outcome.bytes} bytes)")

                BlobOutcome.Absent -> Timber.tag("Recovery")
                    .e("identity restored via Block Store but NO backup blob on disk — history is EMPTY")

                is BlobOutcome.Failed -> Timber.tag("Recovery")
                    .e("identity restored via Block Store but the blob FAILED to import (${outcome.reason}) — history is EMPTY")
            }
            true
        } catch (e: Exception) {
            Timber.tag("Recovery").w(e, "escrowed isk rejected")
            false
        }
    }

    /** Throws only when the identity itself fails to restore. */
    suspend fun restoreFromPhrase(
        context: Context, words: List<String>, name: String,
    ): BlobOutcome {
        CoreBridge.restoreFromPhrase(words, name)
        val outcome = importBlobIfPresent(context)
        escrow(context)
        return outcome
    }

    private suspend fun importBlobIfPresent(context: Context): BlobOutcome {
        val file = blobFile(context)
        if (!file.exists()) {
            Timber.tag("Recovery").w("no backup blob at ${file.absolutePath}")
            return BlobOutcome.Absent
        }
        return try {
            val bytes = file.length()
            CoreBridge.backupImport(file.readBytes())
            Timber.tag("Recovery").i("backup blob imported ($bytes bytes)")
            BlobOutcome.Imported(bytes)
        } catch (e: Exception) {
            Timber.tag("Recovery").e(e, "backup blob import failed")
            BlobOutcome.Failed(e.message ?: e::class.simpleName ?: "unknown error")
        }
    }
}
