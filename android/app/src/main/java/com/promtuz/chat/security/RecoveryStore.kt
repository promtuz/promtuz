package com.promtuz.chat.security

import android.content.Context
import com.google.android.gms.auth.blockstore.Blockstore
import com.google.android.gms.auth.blockstore.RetrieveBytesRequest
import com.google.android.gms.auth.blockstore.StoreBytesData
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.tasks.await
import timber.log.Timber
import java.io.File

/**
 * Identity recovery, platform side (IDENTITY_RECOVERY.md §7).
 *
 * Channel A = Block Store: the raw isk, escrowed to the platform (E2E to
 * Google, lock-screen keyed, survives reinstall + device-to-device restore).
 * Channel B = the BIP39 phrase (RestorePhraseScreen).
 *
 * The backup blob rides Android Auto Backup: [BackupWorker] writes it to
 * `files/recovery/backup.pzbk`, the OS ships that one file to the user's
 * Drive app data (see data_extraction_rules.xml) and restores it BEFORE
 * first launch on reinstall — so by the time either channel restores the
 * identity, the blob is already on disk.
 *
 * Everything here is best-effort: a de-Googled phone fails Block Store and
 * Auto Backup silently — Channel B still works, per the spec's channel split.
 */
object RecoveryStore {
    private const val BS_KEY = "promtuz.isk"

    /** Overwritten by backup_import when a blob exists; user-facing only until then. */
    private const val PLACEHOLDER_NAME = "Restored"

    fun blobFile(context: Context) = File(context.filesDir, "recovery/backup.pzbk")

    /** Escrow the isk into Block Store. Call after enroll and after any restore. */
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
            // No GMS / no lock screen / transient — Channel B still covers the user.
            Timber.tag("Recovery").w(e, "Block Store escrow failed")
        }
    }

    /**
     * Channel A silent restore on fresh launch: Block Store hit → adopt the
     * isk → import the Auto-Backup-restored blob if present. Returns true if
     * the identity was restored (caller navigates into the app).
     */
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
            importBlobIfPresent(context)
            Timber.tag("Recovery").i("identity restored via Block Store")
            true
        } catch (e: Exception) {
            Timber.tag("Recovery").w(e, "escrowed isk rejected")
            false
        }
    }

    /** Channel B restore: typed phrase + prompted name → identity → blob → re-escrow. */
    suspend fun restoreFromPhrase(context: Context, words: List<String>, name: String) {
        CoreBridge.restoreFromPhrase(words, name)
        importBlobIfPresent(context)
        escrow(context)
    }

    /** Feed the Auto-Backup-restored blob to core, if one landed. Best-effort. */
    private suspend fun importBlobIfPresent(context: Context) {
        val file = blobFile(context)
        if (!file.exists()) return
        try {
            CoreBridge.backupImport(file.readBytes())
            Timber.tag("Recovery").i("backup blob imported (${file.length()} bytes)")
        } catch (e: Exception) {
            Timber.tag("Recovery").w(e, "backup blob import failed")
        }
    }
}
