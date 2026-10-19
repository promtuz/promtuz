package com.promtuz.chat.utils.media

import android.content.Context
import android.graphics.BitmapFactory
import android.graphics.ImageDecoder
import android.net.Uri
import android.os.Build
import android.os.ParcelFileDescriptor
import android.provider.OpenableColumns
import uniffi.core.CoreException
import uniffi.core.DecodedImageFrameRecord
import uniffi.core.ImageFrameRequestRecord
import uniffi.core.ImageProbeRecord
import uniffi.core.MediaSourceAdapter
import uniffi.core.VideoProbeRecord
import uniffi.core.VideoFrameRequestRecord
import uniffi.core.VideoRenderReportRecord
import uniffi.core.VideoRenderRequestRecord
import java.io.File
import java.nio.ByteBuffer
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.runBlocking

class MediaPreparationException(message: String) : Exception(message)

/** A bounded URI lease and platform decoder. Core still owns media policy and output. */
class AndroidMediaAdapter private constructor(
    private val context: Context,
    private val uri: Uri,
    descriptor: ParcelFileDescriptor,
    private val input: ParcelFileDescriptor.AutoCloseInputStream,
    private val privateSource: File? = null,
) : MediaSourceAdapter {
    private val lock = Any()
    private val cancelled = AtomicBoolean(false)
    private var released = false
    private var videoFrames: AndroidVideoAdapter.VideoFrameDecoder? = null
    private val sourceSize = descriptor.statSize.takeIf { it >= 0 }?.toULong()
        ?: context.contentResolver.query(uri, arrayOf(OpenableColumns.SIZE), null, null, null)?.use { cursor ->
            if (cursor.moveToFirst() && !cursor.isNull(0)) cursor.getLong(0).takeIf { it >= 0 }?.toULong() else null
        }

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

    override fun inspectImage(): ImageProbeRecord = synchronized(lock) {
        checkOpen()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            var width = 0
            var height = 0
            var hdr = false
            val bitmap = try {
                ImageDecoder.decodeBitmap(ImageDecoder.createSource(context.contentResolver, uri)) { decoder, info, _ ->
                    width = info.size.width
                    height = info.size.height
                    hdr = isHdrColorSpace(info.colorSpace)
                    decoder.allocator = ImageDecoder.ALLOCATOR_SOFTWARE
                    decoder.setTargetSize(1, 1)
                }
            } catch (_: Exception) {
                throw CoreException.Refused("This image format is not supported on this device.")
            }
            hdr = hdr || bitmapHasHdr(bitmap)
            bitmap.recycle()
            if (width <= 0 || height <= 0) throw CoreException.Refused("This image has invalid dimensions.")
            ImageProbeRecord(width.toUInt(), height.toUInt(), false, hdr)
        } else {
            val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
            context.contentResolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it, null, bounds) }
            if (bounds.outWidth <= 0 || bounds.outHeight <= 0) {
                throw CoreException.Refused("This image format is not supported on this device.")
            }
            ImageProbeRecord(bounds.outWidth.toUInt(), bounds.outHeight.toUInt(), false, false)
        }
    }

    override fun decodeImage(request: ImageFrameRequestRecord): DecodedImageFrameRecord = synchronized(lock) {
        checkOpen()
        if (request.maxEdge == 0u) throw CoreException.Internal("invalid image decode request")
        val bitmap = decodeDownscaledBlocking(context, uri, request.maxEdge.toInt())
            ?: throw CoreException.Refused("This image format is not supported on this device.")
        try {
            DecodedImageFrameRecord(
                bitmap.toRgba(),
                bitmap.width.toUInt(),
                bitmap.height.toUInt(),
                false,
                bitmapHasHdr(bitmap),
            )
        } finally {
            bitmap.recycle()
        }
    }

    override fun inspectVideo(): VideoProbeRecord {
        synchronized(lock) { checkOpen() }
        return codecCall { AndroidVideoAdapter.inspect(context, uri) }
    }

    override fun openVideoFrame(request: VideoFrameRequestRecord): DecodedImageFrameRecord = synchronized(lock) {
        checkOpen()
        if (request.maxEdge == 0u) throw CoreException.Internal("invalid video frame request")
        codecCall {
            val decoder = videoFrames ?: AndroidVideoAdapter.VideoFrameDecoder.open(context, uri)
                .also { videoFrames = it }
            val bitmap = decoder.frame(request.timestampUs, request.maxEdge.toInt())
            try {
                DecodedImageFrameRecord(
                    bitmap.toRgba(),
                    bitmap.width.toUInt(),
                    bitmap.height.toUInt(),
                    false,
                    bitmapHasHdr(bitmap),
                )
            } finally {
                bitmap.recycle()
            }
        }
    }

    override fun renderVideo(request: VideoRenderRequestRecord): VideoRenderReportRecord {
        synchronized(lock) { checkOpen() }
        return codecCall {
            val input = AndroidVideoAdapter.inspect(context, uri)
            val output = File(request.outputPath)
            AndroidVideoAdapter.render(
                context, uri, output, input, request.plan, request.trim, cancelled::get,
            )
            val probe = AndroidVideoAdapter.inspect(context, Uri.fromFile(output))
            val poster = videoPoster(context, Uri.fromFile(output), 320)
                ?: throw CoreException.Refused("The prepared video has no readable poster frame.")
            val bitmap = poster.first
            try {
                VideoRenderReportRecord(
                    probe,
                    DecodedImageFrameRecord(
                        bitmap.toRgba(),
                        bitmap.width.toUInt(),
                        bitmap.height.toUInt(),
                        false,
                        false,
                    ),
                    input.audio != null && probe.audio == null,
                )
            } finally {
                bitmap.recycle()
            }
        }
    }

    override fun release() {
        val frames = synchronized(lock) {
            if (released) return
            released = true
            val frames = videoFrames
            videoFrames = null
            runCatching { input.close() }
            frames
        }
        runCatching { frames?.close() }
        privateSource?.delete()
    }

    override fun cancel() {
        cancelled.set(true)
    }

    private fun checkOpen() {
        if (released) throw CoreException.Refused("The selected file is no longer available.")
    }

    private fun <T> codecCall(block: suspend () -> T): T = try {
        runBlocking { block() }
    } catch (error: VideoPreparationException) {
        throw CoreException.Refused(error.message ?: "This video could not be prepared.")
    } catch (error: CoreException) {
        throw error
    } catch (error: Exception) {
        throw CoreException.Refused("This video could not be prepared.")
    }

    companion object {
        private val READ_CHUNK_BYTES = 1_048_576u

        fun open(context: Context, uri: Uri): AndroidMediaAdapter {
            return open(context, uri, null)
        }

        /** Makes clipboard bytes an owned platform source; core still reads it in bounded chunks. */
        fun openBytes(context: Context, bytes: ByteArray): AndroidMediaAdapter {
            if (bytes.size > MAX_BYTE_SOURCE_BYTES) {
                throw MediaPreparationException("This media is too large.")
            }
            val privateSource = File.createTempFile("media-import-", ".source", context.cacheDir)
            return try {
                privateSource.outputStream().use { it.write(bytes) }
                open(context, Uri.fromFile(privateSource), privateSource)
            } catch (error: Exception) {
                privateSource.delete()
                throw MediaPreparationException("Couldn’t open this media.")
            }
        }

        private fun open(context: Context, uri: Uri, privateSource: File?): AndroidMediaAdapter {
            val descriptor = context.contentResolver.openFileDescriptor(uri, "r")
                ?: throw MediaPreparationException("Couldn’t open this media.")
            return try {
                AndroidMediaAdapter(
                    context.applicationContext,
                    uri,
                    descriptor,
                    ParcelFileDescriptor.AutoCloseInputStream(descriptor),
                    privateSource,
                )
            } catch (_: Exception) {
                descriptor.close()
                privateSource?.delete()
                throw MediaPreparationException("Couldn’t open this media.")
            }
        }

        private const val MAX_BYTE_SOURCE_BYTES = 100 * 1024 * 1024
    }
}
