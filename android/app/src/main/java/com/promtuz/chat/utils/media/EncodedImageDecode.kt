package com.promtuz.chat.utils.media

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.ColorSpace
import android.graphics.ImageDecoder
import android.os.Build
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.graphics.asImageBitmap
import com.promtuz.core.CoreBridge
import timber.log.Timber
import java.io.File
import java.nio.ByteBuffer
import kotlin.math.abs
import kotlin.math.roundToInt

internal const val MAX_ENCODED_IMAGE_BYTES = 32 * 1024 * 1024
private const val MAX_SOURCE_PIXELS = 64_000_000L
private const val MAX_SOURCE_EDGE = 16_384

/** Existing image attachments can exceed the inline/native byte budget. Decode them as a file. */
internal fun prepareLargeImageFile(file: File, targetEdge: Int): PreparedEncodedImage? = try {
    require(targetEdge > 0)
    fun validate(width: Int, height: Int) {
        require(width in 1..MAX_SOURCE_EDGE && height in 1..MAX_SOURCE_EDGE &&
            width.toLong() * height <= MAX_SOURCE_PIXELS) { "Image dimensions exceed limit" }
    }
    val bitmap = if (Build.VERSION.SDK_INT >= 28) {
        ImageDecoder.decodeBitmap(ImageDecoder.createSource(file)) { decoder, info, _ ->
            validate(info.size.width, info.size.height)
            decoder.allocator = ImageDecoder.ALLOCATOR_SOFTWARE
            val (width, height) = scaledDimensions(info.size.width, info.size.height, targetEdge)
            decoder.setTargetSize(width, height)
        }
    } else {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeFile(file.absolutePath, bounds)
        validate(bounds.outWidth, bounds.outHeight)
        val options = BitmapFactory.Options().apply {
            var sample = 1
            while (maxOf(bounds.outWidth, bounds.outHeight) / (sample * 2) >= targetEdge) sample *= 2
            inSampleSize = sample
            inPreferredConfig = bounds.outConfig ?: Bitmap.Config.ARGB_8888
        }
        BitmapFactory.decodeFile(file.absolutePath, options) ?: error("Image decode failed")
    }
    PreparedEncodedImage(bitmap.asImageBitmap(), null, bitmapHasHdr(bitmap))
} catch (error: Exception) {
    Timber.tag("MediaImage").d(error, "Image attachment unavailable")
    null
}

/** No native decoder is retained here. A visible player owns and releases its own session. */
internal class PreparedEncodedImage(
    val poster: ImageBitmap,
    val animation: AvifAnimationSource?,
    val hdr: Boolean,
) {
    val allocationBytes: Int
        get() = imageAllocationBytes(poster) + (animation?.encoded?.size ?: 0)
}

internal class AvifAnimationSource(
    val encoded: ByteArray,
    val width: Int,
    val height: Int,
    val decodedPixelCount: Long,
)

internal fun imageAllocationBytes(image: ImageBitmap): Int {
    val bitmap = image.asAndroidBitmap()
    val gainmapBytes = if (Build.VERSION.SDK_INT >= 34) {
        bitmap.gainmap?.gainmapContents?.allocationByteCount ?: 0
    } else 0
    return bitmap.allocationByteCount + gainmapBytes
}

internal fun isHdrColorSpace(colorSpace: ColorSpace?): Boolean {
    if (Build.VERSION.SDK_INT < 34 || colorSpace !is ColorSpace.Rgb) return false
    val hlg = ColorSpace.get(ColorSpace.Named.BT2020_HLG) as ColorSpace.Rgb
    val pq = ColorSpace.get(ColorSpace.Named.BT2020_PQ) as ColorSpace.Rgb
    // Android intentionally returns null transferParameters for these non-ICC transfer curves.
    if (colorSpace.id == hlg.id || colorSpace.id == pq.id) return true
    if (colorSpace.id != ColorSpace.MIN_ID) return false
    // Native decoders can supply a custom gamut with the same HDR transfer. Compare its public
    // EOTF, allowing the float precision used by the native color-space representation.
    return sameTransferCurve(colorSpace, hlg) || sameTransferCurve(colorSpace, pq)
}

private fun sameTransferCurve(actual: ColorSpace.Rgb, expected: ColorSpace.Rgb): Boolean =
    doubleArrayOf(0.0, 0.0625, 0.125, 0.25, 0.5, 0.75, 1.0).all { encoded ->
        val reference = expected.eotf.applyAsDouble(encoded)
        val value = actual.eotf.applyAsDouble(encoded)
        value.isFinite() && reference.isFinite() &&
            abs(value - reference) <= maxOf(1e-7, abs(reference) * 1e-5)
    }

/** Classify the decoded pixels/profile, not a source header that the platform may have flattened. */
internal fun bitmapHasHdr(bitmap: Bitmap): Boolean =
    (Build.VERSION.SDK_INT >= 34 && bitmap.hasGainmap()) || isHdrColorSpace(bitmap.colorSpace)

/** Prefer bundled decoding for ordinary AVIFs; retain platform HDR, gain maps and color profiles. */
internal fun prepareEncodedImage(
    bytes: ByteArray,
    sourceMaxEdge: Int = MAX_SOURCE_EDGE,
    targetEdge: Int = 2048,
    animation: Boolean = true,
): PreparedEncodedImage? {
    if (bytes.isEmpty() || bytes.size > MAX_ENCODED_IMAGE_BYTES || targetEdge < 1) return null
    val avif = looksLikeAvif(bytes)
    val metadata = if (avif) runCatching { CoreBridge.inspectAvif(bytes) }.getOrNull() else null
    // Inspection bounds coded planes before any decoder can allocate them, including images
    // whose small clean-aperture/display dimensions hide much larger coded frames.
    if (avif && metadata == null) return null
    val sourceEdge = minOf(sourceMaxEdge, MAX_SOURCE_EDGE)
    if (metadata != null && (metadata.maxCodedEdge > sourceEdge.toUInt() ||
            metadata.decodedPixelCount > MAX_SOURCE_PIXELS.toULong())) return null
    if (avif) {
        val poster = decodeAvif(bytes, targetEdge) ?: return null
        val sequence = metadata?.takeIf { animation && it.animated && it.frameCount in 2u..600u }?.let {
            AvifAnimationSource(bytes, it.width.toInt(), it.height.toInt(), it.decodedPixelCount.toLong())
        }
        return PreparedEncodedImage(poster, sequence, false)
    }
    var invalidDimensions = false
    fun checkDimensions(width: Int, height: Int) {
        if (width !in 1..sourceEdge || height !in 1..sourceEdge ||
            width.toLong() * height > MAX_SOURCE_PIXELS) {
            invalidDimensions = true
            error("Image dimensions exceed limit")
        }
    }

    var platformAttempted = false
    fun decodePlatformPoster(): Bitmap? {
        if (platformAttempted || invalidDimensions) return null
        platformAttempted = true
        return try {
            if (Build.VERSION.SDK_INT >= 28) {
                ImageDecoder.decodeBitmap(ImageDecoder.createSource(ByteBuffer.wrap(bytes))) { decoder, info, _ ->
                    checkDimensions(info.size.width, info.size.height)
                    // A software bitmap remains usable in notification/share preview canvases. This
                    // does not force ARGB_8888, sRGB, or removal of the Android 14+ gain map.
                    decoder.allocator = ImageDecoder.ALLOCATOR_SOFTWARE
                    val (width, height) = scaledDimensions(info.size.width, info.size.height, targetEdge)
                    if (width != info.size.width || height != info.size.height) {
                        decoder.setTargetSize(width, height)
                    }
                }
            } else {
                val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
                BitmapFactory.decodeByteArray(bytes, 0, bytes.size, bounds)
                require(bounds.outWidth > 0 && bounds.outHeight > 0) { "Platform image decoder unavailable" }
                checkDimensions(bounds.outWidth, bounds.outHeight)
                val options = BitmapFactory.Options().apply {
                    var sample = 1
                    while (maxOf(bounds.outWidth, bounds.outHeight) / (sample * 2) >= targetEdge) sample *= 2
                    inSampleSize = sample
                    inPreferredConfig = bounds.outConfig ?: Bitmap.Config.ARGB_8888
                }
                BitmapFactory.decodeByteArray(bytes, 0, bytes.size, options)?.let { full ->
                    val (width, height) = scaledDimensions(full.width, full.height, targetEdge)
                    if (width == full.width && height == full.height) full else {
                        Bitmap.createScaledBitmap(full, width, height, true).also { full.recycle() }
                    }
                }
            }
        } catch (_: Exception) {
            null
        }
    }
    var poster = decodePlatformPoster()
    fun preparedPoster(): PreparedEncodedImage? {
        if (invalidDimensions) return null
        val image = poster ?: decodePlatformPoster() ?: return null
        return PreparedEncodedImage(image.asImageBitmap(), null, bitmapHasHdr(image))
    }
    if (invalidDimensions) return null

    return preparedPoster()
}

internal fun scaledDimensions(width: Int, height: Int, edge: Int): Pair<Int, Int> {
    val scale = minOf(1.0, edge.toDouble() / maxOf(width, height))
    return (width * scale).roundToInt().coerceAtLeast(1) to (height * scale).roundToInt().coerceAtLeast(1)
}

private fun looksLikeAvif(bytes: ByteArray): Boolean {
    if (bytes.size < 16 || !bytes.matchesAscii(4, "ftyp")) return false
    val size = ((bytes[0].toLong() and 255) shl 24) or ((bytes[1].toLong() and 255) shl 16) or
        ((bytes[2].toLong() and 255) shl 8) or (bytes[3].toLong() and 255)
    val end = minOf(size, bytes.size.toLong()).toInt()
    return (8 until end - 3 step 4).any { at ->
        at != 12 && (bytes.matchesAscii(at, "avif") || bytes.matchesAscii(at, "avis"))
    }
}

private fun ByteArray.matchesAscii(offset: Int, value: String): Boolean =
    offset + value.length <= size && value.indices.all { this[offset + it].toInt() == value[it].code }
