package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.net.Uri
import android.provider.OpenableColumns
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.security.RecoveryStore
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import timber.log.Timber
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/** Severity of one console line; drives its colour in the screen. */
enum class BackupLogLevel { STEP, INFO, OK, WARN, ERR }

data class BackupLogLine(val id: Long, val level: BackupLogLevel, val text: String)

/**
 * Developer utility behind Settings → Developer → Backup & Restore.
 *
 * Two operations, both narrated line-by-line into [console] because the whole
 * point of the screen is to see what the backup pipeline actually does — the
 * production path (RecoveryStore) is deliberately silent and that silence is
 * exactly what makes a failed restore look like a successful one.
 *
 * **Restore is additive.** It goes through `backup_import_merge`, which
 * inserts only rows we don't already hold: nothing existing is replaced,
 * deleted or renamed. The blob decrypts under a key its owner holds, so its
 * plaintext is editable by that owner and must never outrank a live row.
 */
class BackupRestoreVM(private val application: Application) : ViewModel() {

    private val _console = MutableStateFlow<List<BackupLogLine>>(emptyList())
    val console: StateFlow<List<BackupLogLine>> = _console.asStateFlow()

    private val _busy = MutableStateFlow(false)
    val busy: StateFlow<Boolean> = _busy.asStateFlow()

    /** Size of the last snapshot taken this session; gates the "save a copy" action. */
    private val _snapshotBytes = MutableStateFlow<Int?>(null)
    val snapshotBytes: StateFlow<Int?> = _snapshotBytes.asStateFlow()

    private var nextId = 0L

    init {
        line(BackupLogLevel.INFO, "Ready. Blob path: ${blobFile().absolutePath}")
        describeExistingBlob()
    }

    /** Suggested filename for the save-a-copy picker. */
    fun suggestedFileName(): String =
        "promtuz-backup-${SimpleDateFormat("yyyyMMdd-HHmmss", Locale.ENGLISH).format(Date())}.pzbk"

    fun clearConsole() {
        _console.value = emptyList()
        line(BackupLogLevel.INFO, "Console cleared.")
    }

    /**
     * Take a fresh snapshot and write it to the canonical
     * `files/recovery/backup.pzbk` — the same file BackupWorker maintains and
     * Android Auto Backup ships, so this exercises the real pipeline.
     */
    fun snapshot() = run("Snapshot") {
        val target = blobFile()
        line(BackupLogLevel.INFO, "Target: ${target.absolutePath}")
        line(
            BackupLogLevel.INFO,
            if (target.exists()) "Existing blob: ${target.length()} bytes (will be replaced)"
            else "Existing blob: none",
        )

        line(BackupLogLevel.STEP, "Calling core backup_export()…")
        val blob = CoreBridge.backupExport()
        line(BackupLogLevel.OK, "Core returned ${blob.size} bytes")
        describeHeader(blob)

        withContext(Dispatchers.IO) {
            target.parentFile?.mkdirs()
            // Same atomic swap BackupWorker uses, so a crash mid-write can
            // never leave a half-blob where a good one used to be.
            val tmp = File(target.parentFile, "${target.name}.tmp")
            tmp.writeBytes(blob)
            if (!tmp.renameTo(target)) {
                tmp.delete()
                error("atomic rename failed — blob left untouched")
            }
        }
        _snapshotBytes.value = blob.size
        line(BackupLogLevel.OK, "Wrote ${blob.size} bytes (tmp → atomic rename)")
        line(BackupLogLevel.INFO, "Use \"Save a copy\" to export this file off-device.")
    }

    /** Copy the on-disk blob to a user-chosen location via the SAF picker. */
    fun saveCopyTo(uri: Uri) = run("Save a copy") {
        val source = blobFile()
        if (!source.exists()) {
            line(BackupLogLevel.ERR, "No blob at ${source.absolutePath} — take a snapshot first.")
            return@run
        }
        val bytes = withContext(Dispatchers.IO) { source.readBytes() }
        line(BackupLogLevel.INFO, "Read ${bytes.size} bytes from the app's copy")
        withContext(Dispatchers.IO) {
            application.contentResolver.openOutputStream(uri)?.use { it.write(bytes) }
                ?: error("could not open the chosen destination for writing")
        }
        line(BackupLogLevel.OK, "Copied ${bytes.size} bytes to ${displayName(uri)}")
    }

    /**
     * Merge a user-picked `.pzbk` into the live DBs. Additive only — see the
     * class doc. Decryption is keyed on the current identity, so a blob from a
     * different identity fails authentication rather than importing garbage.
     */
    fun restoreFrom(uri: Uri) = run("Restore") {
        line(BackupLogLevel.INFO, "Source: ${displayName(uri)}")
        val blob = withContext(Dispatchers.IO) {
            application.contentResolver.openInputStream(uri)?.use { it.readBytes() }
                ?: error("could not open the chosen file for reading")
        }
        line(BackupLogLevel.OK, "Read ${blob.size} bytes")
        describeHeader(blob)

        line(BackupLogLevel.STEP, "Calling core backup_import_merge()…")
        line(BackupLogLevel.INFO, "Decrypting under HKDF(isk, \"promtuz-backup-v1\")…")
        val r = CoreBridge.backupImportMerge(blob)

        line(BackupLogLevel.OK, "Authenticated and decompressed. Payload:")
        line(
            BackupLogLevel.INFO,
            "  contacts   ${r.contactsAdded} added / ${r.contactsInBlob} in blob" +
                skipped(r.contactsInBlob, r.contactsAdded),
        )
        line(
            BackupLogLevel.INFO,
            "  messages   ${r.messagesAdded} added / ${r.messagesInBlob} in blob" +
                skipped(r.messagesInBlob, r.messagesAdded),
        )
        line(
            BackupLogLevel.INFO,
            "  reactions  ${r.reactionsAdded} added / ${r.reactionsInBlob} in blob" +
                skipped(r.reactionsInBlob, r.reactionsAdded),
        )
        if (r.backupName != r.currentName) {
            line(
                BackupLogLevel.WARN,
                "Display name in blob is \"${r.backupName}\", live is \"${r.currentName}\" — " +
                    "left as-is (a merge never renames).",
            )
        } else {
            line(BackupLogLevel.INFO, "Display name matches (\"${r.currentName}\").")
        }
        line(BackupLogLevel.OK, "Merge complete. No existing row was modified or deleted.")
        line(
            BackupLogLevel.WARN,
            "Not carried by this format: attachments/images, read state, MLS group state.",
        )
    }

    /** `n in blob, m added` → a note about the difference, when there is one. */
    private fun skipped(inBlob: UInt, added: UInt): String {
        val n = inBlob.toLong() - added.toLong()
        return if (n > 0) "  ($n already present, kept)" else ""
    }

    private fun blobFile() = RecoveryStore.blobFile(application)

    private fun describeExistingBlob() {
        val f = blobFile()
        if (!f.exists()) {
            line(BackupLogLevel.WARN, "No blob on disk yet — take a snapshot to create one.")
            return
        }
        val stamp = SimpleDateFormat("yyyy-MM-dd HH:mm:ss", Locale.ENGLISH).format(Date(f.lastModified()))
        line(BackupLogLevel.INFO, "On disk: ${f.length()} bytes, modified $stamp")
    }

    /** Parse the `PZBK ‖ version` header locally so a wrong file fails clearly. */
    private fun describeHeader(blob: ByteArray) {
        if (blob.size < 5) {
            line(BackupLogLevel.ERR, "Too short to be a backup blob (${blob.size} bytes)")
            return
        }
        val magic = String(blob, 0, 4, Charsets.US_ASCII)
        if (magic != "PZBK") {
            line(BackupLogLevel.ERR, "Bad magic \"$magic\" — expected PZBK. Not a backup file.")
            return
        }
        line(BackupLogLevel.INFO, "Header: magic PZBK, version ${blob[4].toInt()}, 24-byte nonce")
    }

    private fun displayName(uri: Uri): String = runCatching {
        application.contentResolver.query(uri, null, null, null, null)?.use { c ->
            val i = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
            if (i >= 0 && c.moveToFirst()) c.getString(i) else null
        }
    }.getOrNull() ?: uri.lastPathSegment ?: uri.toString()

    /** One-at-a-time guard + uniform start/failure narration for every action. */
    private fun run(label: String, block: suspend () -> Unit) {
        if (_busy.value) {
            line(BackupLogLevel.WARN, "Busy — $label ignored.")
            return
        }
        _busy.value = true
        viewModelScope.launch {
            line(BackupLogLevel.STEP, "── $label ──")
            try {
                block()
            } catch (e: Exception) {
                Timber.tag(TAG).w(e, "%s failed", label)
                line(BackupLogLevel.ERR, "$label failed: ${e.message ?: e::class.simpleName}")
            } finally {
                _busy.value = false
            }
        }
    }

    private fun line(level: BackupLogLevel, text: String) {
        _console.value += BackupLogLine(nextId++, level, text)
    }

    private companion object {
        const val TAG = "BackupRestore"
    }
}
