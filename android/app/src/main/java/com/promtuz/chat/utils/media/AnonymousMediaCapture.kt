package com.promtuz.chat.utils.media

import android.os.Build
import android.os.ParcelFileDescriptor
import android.system.Os
import com.sun.jna.Library
import com.sun.jna.Native
import com.sun.jna.NativeLong

/** Native recorders need a real seekable descriptor. This file exists only in kernel RAM. */
internal class AnonymousMediaCapture(private val maxBytes: Long) : PrivateMediaAccess.Source {
    private val descriptor = createDescriptor()
    private var closed = false

    @Synchronized override fun writableDescriptor(): ParcelFileDescriptor {
        check(!closed)
        return ParcelFileDescriptor.dup(descriptor.fileDescriptor)
    }

    @Synchronized override fun size(): Long {
        check(!closed)
        return Os.fstat(descriptor.fileDescriptor).st_size.also { require(it in 0..maxBytes) }
    }

    @Synchronized override fun read(offset: Long, target: ByteArray, count: Int): Int {
        check(!closed)
        require(offset >= 0 && count in 0..target.size)
        val available = (size() - offset).coerceAtLeast(0).coerceAtMost(count.toLong()).toInt()
        return if (available == 0) 0 else Os.pread(descriptor.fileDescriptor, target, 0, available, offset)
    }

    @Synchronized override fun close() {
        if (closed) return
        closed = true
        // Truncate all duplicate descriptors, then forbid the recorder from growing/writing again.
        try {
            runCatching { Os.ftruncate(descriptor.fileDescriptor, 0) }
            runCatching { Os.fcntlInt(descriptor.fileDescriptor, F_ADD_SEALS, F_SEAL_SHRINK or F_SEAL_GROW or F_SEAL_WRITE) }
        } finally { descriptor.close() }
    }

    interface Linux : Library {
        fun syscall(number: NativeLong, name: String, flags: Int): NativeLong
    }

    companion object {
        private const val MFD_CLOEXEC = 1
        private const val MFD_ALLOW_SEALING = 2
        private const val F_ADD_SEALS = 1033
        private const val F_SEAL_SHRINK = 2
        private const val F_SEAL_GROW = 4
        private const val F_SEAL_WRITE = 8

        private fun createDescriptor(): ParcelFileDescriptor {
            val flags = MFD_CLOEXEC or MFD_ALLOW_SEALING
            if (Build.VERSION.SDK_INT >= 30) {
                val fd = Os.memfd_create("locked-capture", flags)
                return try { ParcelFileDescriptor.dup(fd) } finally { Os.close(fd) }
            }
            // The app ships only arm64-v8a and x86_64. Public Java memfd support starts at API 30.
            val number = when (Build.SUPPORTED_ABIS.first()) {
                "arm64-v8a" -> 279L
                "x86_64" -> 319L
                else -> throw UnsupportedOperationException("Anonymous capture unavailable")
            }
            val fd = Native.load("c", Linux::class.java).syscall(NativeLong(number), "locked-capture", flags).toLong()
            check(fd >= 0) { "Couldn’t create anonymous capture" }
            return ParcelFileDescriptor.adoptFd(fd.toInt())
        }
    }
}
