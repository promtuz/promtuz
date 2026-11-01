package com.promtuz.chat.utils.media

import java.io.Closeable
import java.io.File
import java.io.RandomAccessFile
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

/** Seekable capture scratch. Only authenticated ciphertext reaches disk; the key never leaves RAM. */
internal class EncryptedMediaScratch(directory: File, private val authorize: () -> Unit) : Closeable {
    private val path = File.createTempFile("capture-", ".sealed", directory.apply { mkdirs() })
    private val file = RandomAccessFile(path, "rw")
    private val random = SecureRandom()
    private val key = ByteArray(32).also(random::nextBytes)
    private val written = HashSet<Long>()
    private var length = 0L
    private var closed = false

    @Synchronized fun size(): Long { checkOpen(); return length }

    @Synchronized fun read(offset: Long, target: ByteArray, count: Int): Int {
        checkOpen()
        require(offset >= 0 && count in 0..target.size)
        if (offset >= length) return 0
        val available = minOf(count.toLong(), length - offset).toInt()
        var done = 0
        while (done < available) {
            val position = offset + done
            val index = position / BLOCK
            val at = (position % BLOCK).toInt()
            val bytes = decode(index)
            try {
                val take = minOf(available - done, BLOCK - at)
                bytes.copyInto(target, done, at, at + take)
                done += take
            } finally { bytes.fill(0) }
        }
        return done
    }

    @Synchronized fun write(offset: Long, source: ByteArray, count: Int): Int {
        checkOpen()
        require(offset >= 0 && count in 0..source.size && offset <= MAX_BYTES - count)
        var done = 0
        while (done < count) {
            val position = offset + done
            val index = position / BLOCK
            val at = (position % BLOCK).toInt()
            val bytes = decode(index)
            try {
                val take = minOf(count - done, BLOCK - at)
                source.copyInto(bytes, at, done, done + take)
                encode(index, bytes)
                done += take
            } finally { bytes.fill(0) }
        }
        length = maxOf(length, offset + count)
        return count
    }

    private fun decode(index: Long): ByteArray {
        if (index !in written) return ByteArray(BLOCK)
        val nonce = ByteArray(12)
        val encrypted = ByteArray(BLOCK + 16)
        file.seek(index * SLOT)
        file.readFully(nonce)
        file.readFully(encrypted)
        return cipher(Cipher.DECRYPT_MODE, nonce, index).doFinal(encrypted)
    }

    private fun encode(index: Long, bytes: ByteArray) {
        val nonce = ByteArray(12).also(random::nextBytes)
        val encrypted = cipher(Cipher.ENCRYPT_MODE, nonce, index).doFinal(bytes)
        file.seek(index * SLOT)
        file.write(nonce)
        file.write(encrypted)
        written += index
    }

    private fun cipher(mode: Int, nonce: ByteArray, index: Long): Cipher = Cipher.getInstance("AES/GCM/NoPadding").apply {
        init(mode, SecretKeySpec(key, "AES"), GCMParameterSpec(128, nonce))
        updateAAD(java.nio.ByteBuffer.allocate(8).putLong(index).array())
    }

    private fun checkOpen() { check(!closed) { "Capture closed" }; authorize() }

    @Synchronized override fun close() {
        if (closed) return
        closed = true
        key.fill(0)
        written.clear()
        length = 0
        try { file.close() } finally { path.delete() }
    }

    companion object {
        private const val BLOCK = 64 * 1024
        private const val SLOT = BLOCK + 28L
        private const val MAX_BYTES = 2L * 1024 * 1024 * 1024
    }
}
