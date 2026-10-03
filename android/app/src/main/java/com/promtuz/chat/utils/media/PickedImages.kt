package com.promtuz.chat.utils.media

import android.content.Context
import android.graphics.Bitmap
import android.net.Uri
import android.os.Build
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.asAndroidBitmap
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.isActive
import kotlinx.coroutines.withContext
import java.io.ByteArrayOutputStream
import java.io.File
import java.util.UUID

/** Original containers reach core before a still-image decoder can discard their frames or color. */
data class PickedImageSource(
    val preview: ImageBitmap?,
    val rgba: ByteArray? = null,
    val width: Int = 0,
    val height: Int = 0,
    val encoded: ByteArray? = null,
    val preserveOriginal: Boolean = false,
    val bitmap: Bitmap? = null,
)

class ImagePreparationException(message: String) : Exception(message)

private const val EncodedImageMaxBytes = 32 * 1024 * 1024

data class StagedPickedImage(val id: ULong, val preview: ImageBitmap?)

/** Shared by the composer, camera, and Android's share sheet. Core owns a file after staging. */
suspend fun stagePickedImage(context: Context, uri: Uri, allowAttachment: Boolean = true): StagedPickedImage {
    val source = pickImageSource(context, uri, 1600)
    val prepared = source.encoded?.let { CoreBridge.prepareEncodedImage(it) }
    val info = prepared?.let { withContext(Dispatchers.Default) { CoreBridge.inspectAvif(it.bytes) } }
    val extendedColor = info?.let {
        it.hasGainMap || it.hasIcc || it.transferCharacteristics?.toInt() in setOf(16, 18) ||
            (it.colorPrimaries?.toInt()?.let { primaries -> primaries !in setOf(1, 2) } == true)
    } == true
    val attachment = source.preserveOriginal || (prepared != null && (prepared.bytes.size > 256 * 1024 || extendedColor))
    if (attachment && !allowAttachment) {
        throw ImagePreparationException("Send this image as a new message to keep its animation and colors.")
    }
    val preview = source.preview ?: prepared?.let { withContext(Dispatchers.Default) { decodeEncodedPoster(it.bytes, 192) } }
    val caller = currentCoroutineContext()
    caller.ensureActive()
    var staged: ULong? = null
    try {
        // No cancellation gap between creating a private file, handing it to core, and recording
        // its staging id. If the caller goes away meanwhile, discard releases core's retention.
        return withContext(NonCancellable + Dispatchers.IO) {
            val id = when {
                attachment -> {
                    val file = if (prepared == null) resolvePickedFile(context, uri)
                        ?: throw ImagePreparationException("Couldn’t open this image.")
                    else {
                        val dir = File(context.cacheDir, "attachments").apply { mkdirs() }
                        val privateCopy = File(dir, "${UUID.randomUUID()}_image.avif")
                        try { privateCopy.writeBytes(prepared.bytes) }
                        catch (e: Exception) { privateCopy.delete(); throw e }
                        PickedFile(privateCopy.absolutePath, "image.avif", prepared.mime)
                    }
                    try {
                        val poster = preview?.asAndroidBitmap()
                        CoreBridge.stageAttachment(file.path, file.name, file.mime, poster?.toRgba(), poster?.width ?: 0, poster?.height ?: 0)
                    } catch (e: Exception) {
                        File(file.path).delete()
                        throw e
                    }
                }
                prepared != null -> CoreBridge.stageEncodedImage(prepared.bytes)
                else -> CoreBridge.stageImage(source.rgba!!, source.width, source.height)
            }
            staged = id
            StagedPickedImage(id, preview)
        }
    } finally {
        if (!caller.isActive) staged?.let { id ->
            withContext(NonCancellable) { CoreBridge.discardStaged(id) }
        }
    }
}

suspend fun pickImageSource(
    context: Context, uri: Uri, maxEdge: Int, sticker: Boolean = false, keepBitmap: Boolean = false,
): PickedImageSource =
    withContext(Dispatchers.IO) {
        val prefix = context.contentResolver.openInputStream(uri)?.use { input ->
            val bytes = ByteArray(128)
            var used = 0
            while (used < bytes.size) {
                val n = input.read(bytes, used, bytes.size - used)
                if (n < 0) break
                used += n
            }
            bytes.copyOf(used)
        } ?: throw ImagePreparationException("Couldn’t open this image.")
        val avif = prefix.size >= 12 && prefix.ascii(4, 4) == "ftyp" &&
            (8 until prefix.size - 3 step 4).any { prefix.ascii(it, 4) in setOf("avif", "avis") }
        val gif = prefix.ascii(0, 6) in setOf("GIF87a", "GIF89a")
        if (avif || gif) {
            val bytes = readImageBytes(context, uri)
            val info = if (avif) CoreBridge.inspectAvif(bytes) else null
            // A plain eight-bit still can follow the usual sticker resize path. Sequences,
            // higher precision, profiles and gain maps must remain encoded until a preserving
            // transform exists; never turn those into a still bitmap to satisfy a size limit.
            val ordinaryStill = info != null && !info.animated && info.bitDepth.toInt() == 8 &&
                !info.hasGainMap && !info.hasIcc &&
                info.colorPrimaries?.toInt() in setOf(1, 2) &&
                info.transferCharacteristics?.toInt() in setOf(1, 2, 13)
            if (sticker && ordinaryStill &&
                (maxOf(info.width, info.height, info.maxCodedEdge) > maxEdge.toUInt() || bytes.size > 256 * 1024)) {
                val bitmap = decodeEncodedPoster(bytes, maxEdge)?.asAndroidBitmap()
                    ?: throw ImagePreparationException("Couldn’t open this image.")
                val preview = bitmap.preview()
                try {
                    return@withContext PickedImageSource(preview.asImageBitmap(), bitmap.toRgba(), bitmap.width, bitmap.height)
                } finally { if (preview !== bitmap) bitmap.recycle() }
            }
            return@withContext PickedImageSource(
                preview = decodeEncodedPoster(bytes, 192),
                encoded = bytes,
            )
        }

        // Preserve high precision, wide gamut, and gain maps. getPixels() converts to sRGB ints.
        // HEIF remains original on older Android versions which cannot expose its HDR information.
        val mime = context.contentResolver.getType(uri).orEmpty()
        val heif = mime in setOf("image/heic", "image/heif") ||
            (prefix.ascii(4, 4) == "ftyp" && prefix.ascii(8, 4) in setOf("heic", "heix", "hevc", "hevx", "mif1"))
        val extendedData = heif || hasExtendedImageData(context, uri, prefix)
        val bitmap = decodeDownscaled(context, uri, maxEdge)
        if (bitmap == null) {
            if (extendedData) return@withContext PickedImageSource(preview = null, preserveOriginal = true)
            throw ImagePreparationException("Couldn’t open this image.")
        }
        val preview = bitmap.preview()
        val preserve = heif || bitmap.config == Bitmap.Config.RGBA_F16 ||
            (Build.VERSION.SDK_INT >= 33 && bitmap.config == Bitmap.Config.RGBA_1010102) ||
            bitmap.colorSpace?.isSrgb == false ||
            (Build.VERSION.SDK_INT >= 34 && bitmap.hasGainmap()) ||
            extendedData
        if (preserve) {
            if (preview !== bitmap) bitmap.recycle()
            return@withContext PickedImageSource(
                preview = preview.asImageBitmap(), preserveOriginal = true,
            )
        }
        // Crop editors need the source pixels, without a needless RGBA round trip.
        if (keepBitmap) return@withContext PickedImageSource(
            preview.asImageBitmap(), width = bitmap.width, height = bitmap.height, bitmap = bitmap,
        )
        try {
            PickedImageSource(preview.asImageBitmap(), bitmap.toRgba(), bitmap.width, bitmap.height)
        } finally {
            if (preview !== bitmap) bitmap.recycle()
        }
    }

/** Small headers also detect gain-map JPEGs on versions predating Bitmap.hasGainmap(). */
private fun hasExtendedImageData(context: Context, uri: Uri, prefix: ByteArray): Boolean {
    if (prefix.size >= 25 && prefix.ascii(1, 3) == "PNG" && prefix[24].toInt() > 8) return true
    if (prefix.size < 2 || prefix[0] != 0xff.toByte() || prefix[1] != 0xd8.toByte()) return false
    return context.contentResolver.openInputStream(uri)?.buffered()?.use { input ->
        input.read(); input.read()
        var scanned = 2
        while (scanned < 4 * 1024 * 1024) {
            if (input.read() != 0xff) break
            var marker = input.read()
            while (marker == 0xff) marker = input.read()
            if (marker < 0 || marker == 0xda || marker == 0xd9) break
            if (marker == 0x01 || marker in 0xd0..0xd7) continue
            val hi = input.read()
            val lo = input.read()
            if (hi < 0 || lo < 0) break
            val length = (hi shl 8 or lo) - 2
            if (length < 0) break
            val segment = ByteArray(length)
            var used = 0
            while (used < length) {
                val count = input.read(segment, used, length - used)
                if (count < 0) break
                used += count
            }
            if (used != length) break
            scanned += length + 4
            if (marker == 0xe1 || marker == 0xe2) {
                val text = segment.toString(Charsets.ISO_8859_1)
                if (text.contains("hdr-gain-map") || text.contains("hdrgm:") ||
                    text.contains("urn:iso:std:iso:ts:21496:-1") || text.startsWith("MPF\u0000")) return@use true
            }
        }
        false
    } ?: false
}

private fun ByteArray.ascii(offset: Int, length: Int): String =
    if (offset >= 0 && offset + length <= size) String(this, offset, length, Charsets.US_ASCII) else ""

private fun readImageBytes(context: Context, uri: Uri): ByteArray {
    val input = context.contentResolver.openInputStream(uri)
        ?: throw ImagePreparationException("Couldn’t open this image.")
    return input.use {
        val output = ByteArrayOutputStream()
        val buffer = ByteArray(16 * 1024)
        while (true) {
            val n = it.read(buffer)
            if (n < 0) break
            if (output.size().toLong() + n > EncodedImageMaxBytes) {
                throw ImagePreparationException("This image is too large to process. Send it as a file.")
            }
            output.write(buffer, 0, n)
        }
        output.toByteArray()
    }
}

private fun Bitmap.preview(): Bitmap {
    val scale = minOf(1f, 192f / maxOf(width, height))
    return if (scale == 1f) this else Bitmap.createScaledBitmap(
        this, (width * scale).toInt().coerceAtLeast(1), (height * scale).toInt().coerceAtLeast(1), true,
    )
}
