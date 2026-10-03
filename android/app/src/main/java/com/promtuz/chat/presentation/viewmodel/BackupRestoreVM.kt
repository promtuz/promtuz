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

enum class BackupLogLevel { STEP, INFO, OK, WARN, ERR }

data class BackupLogLine(val id: Long, val level: BackupLogLevel, val text: String)

/** Restore only merges: the blob's owner holds its key and can edit its plaintext, so it never
 *  outranks a live row. */
class BackupRestoreVM(private val application: Application) : ViewModel() {

    private val _console = MutableStateFlow<List<BackupLogLine>>(emptyList())
    val console: StateFlow<List<BackupLogLine>> = _console.asStateFlow()

    private val _busy = MutableStateFlow(false)
    val busy: StateFlow<Boolean> = _busy.asStateFlow()

    private var nextId = 0L

    init {
        line(BackupLogLevel.INFO, "Ready. Blob path: ${blobFile().absolutePath}")
        describeExistingBlob()
    }

    fun suggestedFileName(): String =
        "promtuz-backup-${SimpleDateFormat("yyyyMMdd-HHmmss", Locale.ENGLISH).format(Date())}.pzbk"

    fun clearConsole() {
        _console.value = emptyList()
        line(BackupLogLevel.INFO, "Console cleared.")
    }

    /** Writes the same file BackupWorker maintains, so this exercises the real pipeline. */
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
            // Atomic swap, so a crash mid-write never leaves a half-written blob.
            val tmp = File(target.parentFile, "${target.name}.tmp")
            tmp.writeBytes(blob)
            if (!tmp.renameTo(target)) {
                tmp.delete()
                error("atomic rename failed — blob left untouched")
            }
        }
        line(BackupLogLevel.OK, "Wrote ${blob.size} bytes (tmp → atomic rename)")
        line(BackupLogLevel.INFO, "Use \"Save a copy\" to export this file off-device.")
    }

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

    /** A blob from another identity fails authentication rather than importing garbage. */
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
            "  chats      ${r.conversationsAdded} added / ${r.conversationsInBlob} in blob" +
                skipped(r.conversationsInBlob, r.conversationsAdded),
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
        line(
            BackupLogLevel.INFO,
            "  media      ${r.mediaAdded} added / ${r.mediaInBlob} in blob" +
                skipped(r.mediaInBlob, r.mediaAdded),
        )
        // Messages name a chat; without the chats they restore into nothing.
        if (r.messagesInBlob > 0u && r.conversationsInBlob == 0u) {
            line(
                BackupLogLevel.WARN,
                "This blob carries messages but no chats — written before the backup " +
                    "format learned to include them. Its history cannot be reached.",
            )
        }
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
