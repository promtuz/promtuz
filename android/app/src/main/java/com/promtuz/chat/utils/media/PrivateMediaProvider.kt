package com.promtuz.chat.utils.media

import android.content.ContentProvider
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.database.Cursor
import android.database.MatrixCursor
import android.net.Uri
import android.os.Handler
import android.os.HandlerThread
import android.os.ParcelFileDescriptor
import android.os.ProxyFileDescriptorCallback
import android.os.SystemClock
import android.os.storage.StorageManager
import android.provider.OpenableColumns
import android.system.ErrnoException
import android.system.OsConstants
import com.promtuz.chat.security.PrivateChatSession
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import java.io.Closeable
import java.io.FileNotFoundException
import java.util.UUID

/** Revocable URI capabilities. Private media is decrypted only into the caller's read buffer. */
object PrivateMediaAccess {
    private val entries = HashMap<String, Entry>()
    private val internal = HashMap<String, String>()
    private val worker = HandlerThread("private-media").apply { start() }
    private val handler = Handler(worker.looper)
    private var application: Context? = null
    private const val SHARE_MS = 120_000L

    internal interface Source : Closeable {
        fun writableDescriptor(): ParcelFileDescriptor? = null
        fun size(): Long
        fun read(offset: Long, target: ByteArray, count: Int): Int
        fun write(offset: Long, bytes: ByteArray, count: Int): Int = throw SecurityException("Read only")
    }

    internal class Entry(
        val source: Source,
        val name: String,
        val mime: String,
        val conversation: String,
        val session: Long,
        val writable: Boolean,
        val expires: Long? = null,
    ) {
        fun check() {
            if (PrivateChatSession.active.value != conversation || PrivateChatSession.sessionId != session ||
                !CoreBridge.isPrivateConversationUnlocked(conversation.fromHex()) ||
                (expires != null && SystemClock.elapsedRealtime() >= expires)) throw SecurityException("Locked media")
        }
    }

    fun initialize(context: Context) { application = context.applicationContext }

    /** The returned string is an opaque URI, never a plaintext attachment path. */
    fun path(conversation: String, media: uniffi.core.MediaRecord): String? {
        media.localPath?.let { return it }
        val fid = media.fileId ?: return null
        if (media.transferState.toInt() != 2 || PrivateChatSession.active.value != conversation) return null
        val session = PrivateChatSession.sessionId ?: return null
        val key = "$session/$conversation/${fid.toHex()}"
        synchronized(entries) {
            internal[key]?.let { if (entries.containsKey(it)) return uri(it).toString() }
            val reader = try { uniffi.core.openPrivateMedia(conversation.fromHex(), fid) } catch (_: Exception) { return null }
            val token = UUID.randomUUID().toString()
            entries[token] = Entry(object : Source {
                override fun size() = reader.size().toLong()
                override fun read(offset: Long, target: ByteArray, count: Int): Int {
                    val bytes = reader.read(offset.toULong(), minOf(count, 1_048_576).toUInt())
                    try { bytes.copyInto(target); return bytes.size } finally { bytes.fill(0) }
                }
                override fun close() { reader.destroy() }
            }, media.name, media.mime, conversation, session, false)
            internal[key] = token
            return uri(token).toString()
        }
    }

    class Capture internal constructor(val uri: Uri, private val token: String) : Closeable {
        fun descriptor(context: Context): ParcelFileDescriptor {
            val item = entry(uri)
            val native = synchronized(item) {
                item.check()
                item.source.writableDescriptor()
            }
            return native ?: context.contentResolver.openFileDescriptor(uri, "rw") ?: throw FileNotFoundException()
        }
        fun size(): Long = entry(uri).let { it.check(); it.source.size() }
        override fun close() = remove(token)
    }

    fun capture(context: Context, conversation: String, name: String, mime: String, nativeCodec: Boolean = false,
                maxBytes: Long = 256L * 1024 * 1024): Capture {
        initialize(context)
        val session = PrivateChatSession.sessionId ?: throw SecurityException("Locked media")
        val check = {
            check(PrivateChatSession.active.value == conversation && PrivateChatSession.sessionId == session &&
                CoreBridge.isPrivateConversationUnlocked(conversation.fromHex())) { "Locked media" }
        }
        check()
        val source: Source = if (nativeCodec) AnonymousMediaCapture(maxBytes) else {
            val scratch = EncryptedMediaScratch(java.io.File(context.cacheDir, "locked-captures"), check)
            object : Source {
                override fun size() = scratch.size()
                override fun read(offset: Long, target: ByteArray, count: Int) = scratch.read(offset, target, count)
                override fun write(offset: Long, bytes: ByteArray, count: Int) = scratch.write(offset, bytes, count)
                override fun close() = scratch.close()
            }
        }
        val token = UUID.randomUUID().toString()
        try {
            synchronized(entries) { check(); entries[token] = Entry(source, name, mime, conversation, session, true) }
        } catch (error: Exception) { source.close(); throw error }
        return Capture(uri(token), token)
    }

    fun shareBytes(context: Context, bytes: ByteArray, mime: String, name: String): Uri {
        initialize(context)
        val conversation = PrivateChatSession.active.value ?: throw SecurityException("Locked media")
        val session = PrivateChatSession.sessionId ?: throw SecurityException("Locked media")
        val data = bytes.copyOf()
        return addExport(Entry(object : Source {
            @Synchronized override fun size() = data.size.toLong()
            @Synchronized override fun read(offset: Long, target: ByteArray, count: Int): Int {
                if (offset < 0 || offset >= data.size) return 0
                val take = minOf(count, data.size - offset.toInt())
                data.copyInto(target, 0, offset.toInt(), offset.toInt() + take)
                return take
            }
            @Synchronized override fun close() { data.fill(0) }
        }, name, mime, conversation, session, false, SystemClock.elapsedRealtime() + SHARE_MS))
    }

    fun export(context: Context, path: String): Uri {
        initialize(context)
        val original = entry(Uri.parse(path))
        original.check()
        return addExport(Entry(object : Source {
            override fun size(): Long { original.check(); return original.source.size() }
            override fun read(offset: Long, target: ByteArray, count: Int): Int {
                original.check(); return original.source.read(offset, target, count)
            }
            override fun close() = Unit // The original reader belongs to the chat, not this grant.
        }, original.name, original.mime, original.conversation, original.session, false,
            SystemClock.elapsedRealtime() + SHARE_MS))
    }

    private fun addExport(entry: Entry): Uri {
        val token = UUID.randomUUID().toString()
        try { synchronized(entries) { entry.check(); entries[token] = entry } }
        catch (error: Exception) { entry.source.close(); throw error }
        handler.postDelayed({ remove(token) }, SHARE_MS)
        return uri(token)
    }

    internal fun entry(uri: Uri): Entry = synchronized(entries) {
        entries[uri.lastPathSegment] ?: throw FileNotFoundException("Expired media")
    }

    internal fun open(context: Context, uri: Uri, mode: String): ParcelFileDescriptor {
        val item = entry(uri)
        item.check()
        val writing = mode != "r"
        if (writing && (!item.writable || android.os.Binder.getCallingUid() != android.os.Process.myUid())) {
            throw SecurityException("Read only")
        }
        val flags = if (writing) ParcelFileDescriptor.MODE_READ_WRITE else ParcelFileDescriptor.MODE_READ_ONLY
        return context.getSystemService(StorageManager::class.java).openProxyFileDescriptor(flags,
            object : ProxyFileDescriptorCallback() {
                override fun onGetSize(): Long = guarded { item.source.size() }
                override fun onRead(offset: Long, size: Int, data: ByteArray): Int = guarded {
                    try {
                        require(offset >= 0)
                        val count = item.source.read(offset, data, minOf(size, data.size))
                        item.check()
                        count
                    } catch (error: Exception) { data.fill(0); throw error }
                }
                override fun onWrite(offset: Long, size: Int, data: ByteArray): Int = guarded {
                    if (!writing) throw SecurityException("Read only")
                    item.source.write(offset, data, minOf(size, data.size))
                }
                override fun onFsync() { guarded { Unit } }
                override fun onRelease() = Unit
                private fun <T> guarded(block: () -> T): T = synchronized(item) {
                    try { item.check(); block() } catch (_: Exception) {
                        throw ErrnoException("locked media", OsConstants.EACCES)
                    }
                }
            }, handler)
    }

    private fun remove(token: String) {
        val item = synchronized(entries) { entries.remove(token) } ?: return
        runCatching { application?.revokeUriPermission(uri(token), Intent.FLAG_GRANT_READ_URI_PERMISSION) }
        synchronized(item) { runCatching { item.source.close() } }
    }

    fun clear() {
        val tokens = synchronized(entries) { internal.clear(); entries.keys.toList() }
        tokens.forEach(::remove)
    }

    private fun uri(token: String): Uri = Uri.parse("content://${application?.packageName ?: "com.promtuz.chat"}.private-media/$token")
    fun isPrivate(path: String): Boolean = path.startsWith("content://") && path.contains(".private-media/")
}

class PrivateMediaProvider : ContentProvider() {
    override fun onCreate(): Boolean { context?.let(PrivateMediaAccess::initialize); return true }
    override fun openFile(uri: Uri, mode: String): ParcelFileDescriptor = PrivateMediaAccess.open(requireNotNull(context), uri, mode)
    override fun getType(uri: Uri): String = PrivateMediaAccess.entry(uri).let { it.check(); it.mime }
    override fun query(uri: Uri, projection: Array<out String>?, selection: String?, selectionArgs: Array<out String>?, sortOrder: String?): Cursor {
        val item = PrivateMediaAccess.entry(uri)
        item.check()
        val columns = projection ?: arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE)
        return MatrixCursor(columns).apply {
            addRow(columns.map<String, Any?> { when (it) {
                OpenableColumns.DISPLAY_NAME -> item.name
                OpenableColumns.SIZE -> item.source.size()
                else -> null
            } }.toTypedArray())
        }
    }
    override fun insert(uri: Uri, values: ContentValues?): Uri? = throw UnsupportedOperationException()
    override fun update(uri: Uri, values: ContentValues?, selection: String?, selectionArgs: Array<out String>?): Int = throw UnsupportedOperationException()
    override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?): Int = throw UnsupportedOperationException()
}
