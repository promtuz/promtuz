package com.promtuz.chat.utils.media

import java.io.RandomAccessFile
import java.util.concurrent.atomic.AtomicBoolean
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class EncryptedMediaScratchTest {
    @get:Rule val temporary = TemporaryFolder()

    @Test fun overlappingMuxerWritesAreEncryptedAndSparseReadsAreZeroFilled() {
        val directory = temporary.newFolder()
        val marker = "private-capture-must-never-be-a-plaintext-file".toByteArray()
        EncryptedMediaScratch(directory) {}.use { source ->
            val offset = 64 * 1024L - 12
            source.write(offset, marker, marker.size)
            val replacement = "revised-header".toByteArray()
            source.write(offset + 8, replacement, replacement.size)
            val expected = marker.copyOf().apply { replacement.copyInto(this, 8) }
            val result = ByteArray(marker.size)
            assertEquals(marker.size, source.read(offset, result, result.size))
            assertArrayEquals(expected, result)
            val gap = ByteArray(32) { 99 }
            source.read(12, gap, gap.size)
            assertArrayEquals(ByteArray(32), gap)
            val persisted = directory.listFiles()!!.single().readBytes()
            assertFalse(String(persisted, Charsets.ISO_8859_1).contains(String(marker, Charsets.ISO_8859_1)))
            assertEquals(offset + marker.size, source.size())
        }
        assertTrue(directory.listFiles()!!.isEmpty())
    }

    @Test fun revokingAnExistingCaptureDeniesSizeReadAndWriteBeforeDestruction() {
        val allowed = AtomicBoolean(true)
        val source = EncryptedMediaScratch(temporary.newFolder()) { check(allowed.get()) }
        source.write(0, byteArrayOf(1, 2, 3), 3)
        allowed.set(false)
        assertTrue(runCatching { source.size() }.isFailure)
        assertTrue(runCatching { source.read(0, ByteArray(3), 3) }.isFailure)
        assertTrue(runCatching { source.write(0, byteArrayOf(4), 1) }.isFailure)
        source.close()
    }

    @Test fun corruptOrRelocatedCiphertextCannotBeDecoded() {
        val directory = temporary.newFolder()
        EncryptedMediaScratch(directory) {}.use { source ->
            source.write(0, byteArrayOf(1, 2, 3), 3)
            RandomAccessFile(directory.listFiles()!!.single(), "rw").use {
                it.seek(18)
                val old = it.readByte()
                it.seek(18)
                it.writeByte(old.toInt() xor 1)
            }
            assertTrue(runCatching { source.read(0, ByteArray(3), 3) }.isFailure)
        }
    }
}
