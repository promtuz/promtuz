package com.promtuz.chat.utils.media

import android.content.ContentResolver
import android.net.Uri
import android.os.ParcelFileDescriptor
import uniffi.core.CoreException
import uniffi.core.MediaSourceAdapter
import java.nio.ByteBuffer

/** A seekable, bounded lease over a platform-owned media URI. */
class AndroidMediaSource private constructor(
    descriptor: ParcelFileDescriptor,
    private val input: ParcelFileDescriptor.AutoCloseInputStream,
) : MediaSourceAdapter {
    private val lock = Any()
    private var released = false
    private val sourceSize = descriptor.statSize.takeIf { it >= 0 }?.toULong()

    override fun length(): ULong? = synchronized(lock) {
        checkOpen()
        sourceSize
    }

    override fun read(offset: ULong, maxBytes: UInt): ByteArray = synchronized(lock) {
        checkOpen()
        if (offset > Long.MAX_VALUE.toULong() || maxBytes > READ_CHUNK_BYTES) {
            throw CoreException.Internal("invalid media source read")
        }
        val buffer = ByteBuffer.allocate(maxBytes.toInt())
        input.channel.position(offset.toLong())
        while (buffer.hasRemaining()) {
            val read = input.channel.read(buffer)
            if (read <= 0) break
        }
        buffer.array().copyOf(buffer.position())
    }

    override fun release() = synchronized(lock) {
        if (released) return
        released = true
        runCatching { input.close() }
    }

    private fun checkOpen() {
        if (released) throw CoreException.Refused("The selected file is no longer available.")
    }

    companion object {
        private const val READ_CHUNK_BYTES = 1024 * 1024u

        fun open(resolver: ContentResolver, uri: Uri): AndroidMediaSource {
            val descriptor = resolver.openFileDescriptor(uri, "r")
                ?: throw ImagePreparationException("Couldn’t open this media.")
            return try {
                AndroidMediaSource(descriptor, ParcelFileDescriptor.AutoCloseInputStream(descriptor))
            } catch (_: Exception) {
                descriptor.close()
                throw ImagePreparationException("Couldn’t open this media.")
            }
        }
    }
}
