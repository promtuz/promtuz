package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.net.Uri
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

sealed interface BackupOutcome {
    data class Restored(val contacts: Int, val chats: Int, val messages: Int, val unreachable: Boolean) : BackupOutcome
    data class Failed(val title: String, val text: String) : BackupOutcome
}

/** Restore only merges: the blob's owner holds its key and can edit its plaintext, so it never
 *  outranks a live row. */
class BackupRestoreVM(private val application: Application) : ViewModel() {

    private val _lastBackup = MutableStateFlow(lastModified())
    val lastBackup: StateFlow<Long?> = _lastBackup.asStateFlow()

    private val _bytes = MutableStateFlow(blobFile().length())
    val bytes: StateFlow<Long> = _bytes.asStateFlow()

    private val _busy = MutableStateFlow(false)
    val busy: StateFlow<Boolean> = _busy.asStateFlow()

    private val _outcome = MutableStateFlow<BackupOutcome?>(null)
    val outcome: StateFlow<BackupOutcome?> = _outcome.asStateFlow()

    fun dismissOutcome() { _outcome.value = null }

    fun suggestedFileName(): String =
        "promtuz-backup-${SimpleDateFormat("yyyyMMdd-HHmmss", Locale.ENGLISH).format(Date())}.pzbk"

    /** Writes the same file BackupWorker maintains. */
    fun snapshot() = run(BackupOutcome.Failed("Couldn't back up", "Try again in a moment.")) {
        val blob = CoreBridge.backupExport()
        withContext(Dispatchers.IO) {
            val target = blobFile()
            target.parentFile?.mkdirs()
            // Atomic swap, so a crash mid-write never leaves a half-written blob.
            val tmp = File(target.parentFile, "${target.name}.tmp")
            tmp.writeBytes(blob)
            if (!tmp.renameTo(target)) {
                tmp.delete()
                error("atomic rename failed")
            }
        }
        _lastBackup.value = lastModified()
        _bytes.value = blobFile().length()
    }

    fun saveCopyTo(uri: Uri) = run(BackupOutcome.Failed("Couldn't save the copy", "Choose another place and try again.")) {
        withContext(Dispatchers.IO) {
            val bytes = blobFile().readBytes()
            application.contentResolver.openOutputStream(uri)?.use { it.write(bytes) }
                ?: error("could not open the destination")
        }
    }

    /** A blob from another identity fails authentication rather than importing garbage. */
    fun restoreFrom(uri: Uri) = run(BackupOutcome.Failed(
        "Couldn't restore", "This file isn't a Promtuz backup, or it belongs to another identity.",
    )) {
        val blob = withContext(Dispatchers.IO) {
            application.contentResolver.openInputStream(uri)?.use { it.readBytes() }
                ?: error("could not open the file")
        }
        val r = CoreBridge.backupImportMerge(blob)
        _outcome.value = BackupOutcome.Restored(
            contacts = r.contactsAdded.toInt(),
            chats = r.conversationsAdded.toInt(),
            messages = r.messagesAdded.toInt(),
            // Messages name a chat; old blobs without chats restore into nothing.
            unreachable = r.messagesInBlob > 0u && r.conversationsInBlob == 0u,
        )
    }

    private fun blobFile() = RecoveryStore.blobFile(application)

    private fun lastModified(): Long? = blobFile().takeIf { it.exists() }?.lastModified()

    private fun run(failure: BackupOutcome.Failed, block: suspend () -> Unit) {
        if (_busy.value) return
        _busy.value = true
        viewModelScope.launch {
            try {
                block()
            } catch (e: Exception) {
                Timber.tag(TAG).w(e, "%s", failure.title)
                _outcome.value = failure
            } finally {
                _busy.value = false
            }
        }
    }

    private companion object {
        const val TAG = "BackupRestore"
    }
}
