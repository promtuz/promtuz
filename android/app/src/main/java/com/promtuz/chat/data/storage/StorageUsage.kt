package com.promtuz.chat.data.storage

import android.content.Context
import android.os.StatFs
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.emptyFlow
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.File
import java.nio.file.Files
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.chat.utils.extensions.fromHex

internal enum class StorageCategory { Chats, Attachments, Voice, Updates, Backups, Shared, Other }
internal data class StorageEntry(val category: StorageCategory, val bytes: Long)
internal data class StorageUsage(val entries: List<StorageEntry>, val free: Long, val capacity: Long) {
    val total get() = entries.sumOf { it.bytes }
    val shared get() = entries.first { it.category == StorageCategory.Shared }.bytes
}

/** Attachments in cache are owned by core and may still be referenced by messages. */
internal data class StorageMediaItem(
    val conversation: String, val dispatch: String, val chat: String, val kind: Int,
    val name: String, val caption: String, val timestamp: Long, val bytes: Long,
) { val key get() = "$conversation:$dispatch" }

internal interface StorageSource {
    val changes: Flow<Unit> get() = emptyFlow()
    suspend fun read(): StorageUsage
    suspend fun preview(item: StorageMediaItem): ByteArray? = null
    suspend fun media(): List<StorageMediaItem>
    suspend fun remove(items: List<StorageMediaItem>): Int
    suspend fun clearSharedCopies(): Boolean
}

internal class StorageRepository(context: Context) : StorageSource {
    override val changes = com.promtuz.core.adapter.CoreEventBus.dbChanged
        .filter { tables -> tables.any { it in setOf("messages", "message_media", "partials", "conversations", "contacts") } }
        .map { Unit }
    private val context = context.applicationContext
    private val sharedDirectories get() = listOf("images", "logs").map { File(context.cacheDir, it) }

    override suspend fun preview(item: StorageMediaItem) =
        com.promtuz.core.CoreBridge.storagePreview(item.conversation.fromHex(), item.dispatch.fromHex())

    override suspend fun media(): List<StorageMediaItem> {
        val names = com.promtuz.core.CoreBridge.listConversations().associate { it.id.toHex() to it.displayName }
        return com.promtuz.core.CoreBridge.storageMedia().map { item ->
            val conversation = item.conversationId.toHex()
            StorageMediaItem(conversation, item.dispatchId.toHex(), names[conversation] ?: "Chat",
                item.kind.toInt(), item.name, item.caption, item.timestamp.toLong(), item.localBytes.toLong())
        }
    }

    override suspend fun remove(items: List<StorageMediaItem>): Int =
        com.promtuz.core.CoreBridge.removeStoredMedia(items.map {
            uniffi.core.StorageTarget(it.conversation.fromHex(), it.dispatch.fromHex())
        }).toInt()

    override suspend fun read(): StorageUsage = withContext(Dispatchers.IO) {
        val cache = context.cacheDir
        val shared = sharedDirectories.sumOf(::directoryBytes)
        val attachments = directoryBytes(File(cache, "attachments"))
        val voice = directoryBytes(File(cache, "voice"))
        val updates = directoryBytes(File(cache, "updates"))
        val backups = directoryBytes(File(context.filesDir, "recovery"))
        val saved = directoryBytes(context.filesDir) + directoryBytes(context.noBackupFilesDir) +
            directoryBytes(File(context.applicationInfo.dataDir, "databases")) +
            directoryBytes(File(context.applicationInfo.dataDir, "shared_prefs"))
        val disk = StatFs(context.filesDir.absolutePath)
        StorageUsage(listOf(
            StorageEntry(StorageCategory.Chats, (saved - backups).coerceAtLeast(0)),
            StorageEntry(StorageCategory.Attachments, attachments),
            StorageEntry(StorageCategory.Voice, voice),
            StorageEntry(StorageCategory.Updates, updates),
            StorageEntry(StorageCategory.Backups, backups),
            StorageEntry(StorageCategory.Shared, shared),
            StorageEntry(StorageCategory.Other, (directoryBytes(cache) - shared - attachments - voice - updates).coerceAtLeast(0)),
        ), disk.availableBytes, disk.totalBytes)
    }

    override suspend fun clearSharedCopies(): Boolean = withContext(Dispatchers.IO) {
        var complete = true
        sharedDirectories.forEach { directory ->
            if (Files.isSymbolicLink(directory.toPath())) {
                complete = false
                return@forEach
            }
            val files = directory.listFiles()
            if (files == null && directory.exists()) complete = false
            files?.forEach { file -> if (!file.isFile || !file.delete()) complete = false }
        }
        complete
    }
}

private fun directoryBytes(root: File): Long {
    if (!root.exists() || Files.isSymbolicLink(root.toPath())) return 0
    if (root.isFile) return root.length()
    return root.listFiles()?.sumOf(::directoryBytes) ?: error("Unable to read storage")
}
