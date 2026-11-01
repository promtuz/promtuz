package com.promtuz.chat.update

import android.content.Context
import com.promtuz.core.CoreBridge
import java.io.File

/** Device-local install history, separate from account backups and the selected update channel. */
internal class ReleaseNotesStore(context: Context, val code: Long, val version: String, val channel: String) {
    private val prefs = context.getSharedPreferences("installed_releases", Context.MODE_PRIVATE)
    private val directory = File(context.noBackupFilesDir, "release-notes").apply { mkdirs() }
    val channelChanged: Boolean
    val previousCode: Long?
    val previousVersion: String?

    init {
        val oldCode = prefs.getLong("code", -1)
        val oldChannel = prefs.getString("channel", null)
        channelChanged = oldChannel != channel
        if (oldCode != code || oldChannel != channel) {
            val forward = oldCode >= 0 && code > oldCode
            prefs.edit()
                .putLong("previous_code", if (forward) oldCode else -1)
                .putString("previous_version", if (forward) prefs.getString("version", null) else null)
                .putLong("code", code).putString("version", version).putString("channel", channel)
                .commit()
        }
        previousCode = prefs.getLong("previous_code", -1).takeIf { it >= 0 }
        previousVersion = prefs.getString("previous_version", null)
    }

    @Synchronized
    fun read(channel: String, code: Long): ReleaseNote? = runCatching {
        val raw = File(directory, "$channel-$code.md").readBytes()
        val signature = File(directory, "$channel-$code.sig").readBytes()
        require(raw.size <= 64 * 1024 && CoreBridge.verifyUpdateManifest(raw, signature))
        ReleaseNote.parse(code, raw.decodeToString())
    }.getOrNull()

    @Synchronized
    fun write(channel: String, code: Long, raw: ByteArray, signature: ByteArray): ReleaseNote {
        require(CoreBridge.verifyUpdateManifest(raw, signature)) { "Notes signature could not be verified." }
        File(directory, "$channel-$code.sig").writeBytes(signature)
        File(directory, "$channel-$code.md").writeBytes(raw)
        return ReleaseNote.parse(code, raw.decodeToString())
    }

    @Synchronized
    fun cached(channel: String): List<ReleaseNote> = directory.listFiles().orEmpty().mapNotNull { file ->
        file.name.removePrefix("$channel-").takeIf { file.name.startsWith("$channel-") }
            ?.removeSuffix(".md")?.toLongOrNull()?.let { read(channel, it) }
    }.sortedByDescending { it.code }
}

data class InstalledChanges(
    val previousVersion: String?, val version: String, val previousCode: Long?, val code: Long,
    val notes: List<ReleaseNote>,
)
