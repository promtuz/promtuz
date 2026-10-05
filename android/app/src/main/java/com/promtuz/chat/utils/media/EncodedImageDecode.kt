package com.promtuz.chat.utils.media

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.ColorSpace
import android.graphics.ImageDecoder
import android.graphics.Matrix
import android.os.Build
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.graphics.asImageBitmap
import com.promtuz.core.CoreBridge
import org.aomedia.avif.android.AvifDecoder
import timber.log.Timber
import uniffi.core.AvifInfoRecord
import java.io.File
import java.nio.ByteBuffer
import kotlin.math.abs
import kotlin.math.roundToInt

internal const val MAX_ENCODED_IMAGE_BYTES = 32 * 1024 * 1024
private const val MAX_SOURCE_PIXELS = 64_000_000L
private const val MAX_SOURCE_EDGE = 16_384
private const val MAX_NATIVE_POSTER_PIXELS = 16_000_000L

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
        get() = imageAllocationBytes(poster) + (animation?.encoded?.capacity() ?: 0)
}

internal class AvifAnimationSource(
    // JNI holds a raw address into this buffer, so keep it strongly reachable through release().
    val encoded: ByteBuffer,
    val width: Int,
    val height: Int,
    val depth: Int,
    val decodedPixelCount: Long,
    val alpha: Boolean,
    val colorSpace: ColorSpace,
    val rotationQuarterTurns: Int,
    val mirrorAxis: Int?,
) {
    val config: Bitmap.Config
        get() = if (depth > 8 || isHdrColorSpace(colorSpace)) Bitmap.Config.RGBA_F16 else Bitmap.Config.ARGB_8888
}

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
    // The pinned JNI has no setSource(TRACKS). AUTO otherwise selects the primary still item
    // for major brand avif, even when a validated animation is present. This private decode
    // buffer selects its tracks; the caller's original bytes remain untouched for storage/export.
    val sequenceBuffer = if (metadata?.animated == true && bytes.matchesAscii(8, "avif")) {
        ByteBuffer.allocateDirect(bytes.size).put(bytes).apply {
            put(11, 's'.code.toByte()) // avif -> avis, only in the decoder's copy.
            rewind()
        }
    } else null
    var sourceColorSpace: ColorSpace? = null
    var invalidDimensions = false
    fun checkDimensions(width: Int, height: Int) {
        if (width !in 1..sourceEdge || height !in 1..sourceEdge ||
            width.toLong() * height > MAX_SOURCE_PIXELS) {
            invalidDimensions = true
            error("Image dimensions exceed limit")
        }
    }

    // Keep ordinary previews on the same decoder across devices. ICC profiles, gain maps and
    // extended color still need the platform path; the native allocation limit also still applies.
    val preferBundledSdr = metadata?.let {
        !it.hasIcc && !it.hasGainMap &&
            it.colorPrimaries?.toInt() in setOf(1, 2) &&
            it.transferCharacteristics?.toInt() in setOf(1, 2, 6, 13) &&
            it.decodedPixelCount <= MAX_NATIVE_POSTER_PIXELS.toULong()
    } == true
    var platformAttempted = false
    fun decodePlatformPoster(): Bitmap? {
        if (platformAttempted || invalidDimensions) return null
        platformAttempted = true
        return try {
            if (Build.VERSION.SDK_INT >= 28) {
                ImageDecoder.decodeBitmap(ImageDecoder.createSource(sequenceBuffer?.duplicate() ?: ByteBuffer.wrap(bytes))) { decoder, info, _ ->
                    checkDimensions(info.size.width, info.size.height)
                    sourceColorSpace = info.colorSpace
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
                sourceColorSpace = bounds.outColorSpace
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
            null // Bundled libavif also covers Android versions with no platform AVIF decoder.
        }
    }
    var poster = if (preferBundledSdr) null else decodePlatformPoster()
    fun preparedPoster(): PreparedEncodedImage? {
        if (invalidDimensions) return null
        val image = poster ?: decodePlatformPoster() ?: return null
        return PreparedEncodedImage(image.asImageBitmap(), null, bitmapHasHdr(image))
    }
    if (invalidDimensions) return null

    if (!avif) return preparedPoster()
    if (metadata == null) return null
    val sourceIsHdr = metadata.transferCharacteristics?.toInt() in setOf(16, 18)
    // Some Android 16 AVIF codecs report sRGB/ARGB_8888 even for PQ/HLG sources. A successful
    // platform decode is not proof of HDR retention; prefer an explicitly tagged F16 decode.
    val preferNativeHdr = sourceIsHdr && Build.VERSION.SDK_INT >= 34 && poster != null &&
        !bitmapHasHdr(poster)
    if (!animation && poster != null && !preferNativeHdr) {
        return preparedPoster()
    }

    return try {
        val encoded = sequenceBuffer ?: ByteBuffer.allocateDirect(bytes.size).put(bytes).apply { rewind() }
        val decoder = AvifDecoder.create(encoded, 1) ?: return preparedPoster()
        try {
            checkDimensions(decoder.width, decoder.height)
            val nativeDisplayWidth = if (metadata.rotationQuarterTurns.toInt() and 1 != 0) decoder.height else decoder.width
            val nativeDisplayHeight = if (metadata.rotationQuarterTurns.toInt() and 1 != 0) decoder.width else decoder.height
            if (nativeDisplayWidth.toUInt() != metadata.width || nativeDisplayHeight.toUInt() != metadata.height) {
                // The pinned JNI ignores clap values requiring chroma upsampling/fractional crops.
                // Keep the platform's correctly transformed image in those cases.
                return preparedPoster()
            }
            // The JNI adapter copies YUV->RGB without changing the transfer function or gamut.
            // Its output must carry the source profile, never a profile inferred from bit depth.
            val colorSpace = nativeAvifColorSpace(metadata, sourceColorSpace)
            if ((poster == null || preferNativeHdr) && colorSpace != null &&
                metadata.decodedPixelCount <= MAX_NATIVE_POSTER_PIXELS.toULong()) {
                val (width, height) = scaledDimensions(decoder.width, decoder.height, targetEdge)
                val bitmap = Bitmap.createBitmap(width, height,
                    if (decoder.depth > 8 || isHdrColorSpace(colorSpace)) Bitmap.Config.RGBA_F16 else Bitmap.Config.ARGB_8888,
                    decoder.alphaPresent, colorSpace)
                val result = decoder.nextFrame(bitmap)
                if (result == 0) {
                    val previous = poster
                    poster = orientAvifBitmap(bitmap, metadata.rotationQuarterTurns.toInt(),
                        metadata.mirrorAxis?.toInt())
                    previous?.recycle() // Never published: this function still owns both posters.
                } else bitmap.recycle()
            }
            val image = poster ?: return preparedPoster()
            val sequence = if (animation && colorSpace != null && decoder.frameCount in 2..600 &&
                !metadata.hasGainMap && !(Build.VERSION.SDK_INT >= 34 && image.hasGainmap())) {
                AvifAnimationSource(encoded, decoder.width, decoder.height, decoder.depth, metadata.decodedPixelCount.toLong(),
                    decoder.alphaPresent, colorSpace, metadata.rotationQuarterTurns.toInt(),
                    metadata.mirrorAxis?.toInt())
            } else null
            PreparedEncodedImage(image.asImageBitmap(), sequence, bitmapHasHdr(image))
        } finally {
            decoder.release()
        }
    } catch (error: LinkageError) {
        Timber.tag("MediaImage").w(error, "Bundled AVIF decoder unavailable")
        preparedPoster()
    } catch (error: Exception) {
        Timber.tag("MediaImage").d(error, "Encoded image unavailable")
        preparedPoster()
    }
}

/** ICC and HDR transfers need a real source profile; do not relabel their encoded pixels sRGB. */
private fun nativeAvifColorSpace(info: AvifInfoRecord, platform: ColorSpace?): ColorSpace? {
    val primaries = info.colorPrimaries?.toInt()
    val transfer = info.transferCharacteristics?.toInt()
    if (info.hasIcc) return platform
    if (transfer == 16 || transfer == 18) {
        if (isHdrColorSpace(platform)) return platform
        if (Build.VERSION.SDK_INT < 34 || primaries != 9) return null
        // Custom HDR profiles must come from the platform. PQ/HLG have no public ICC parameters,
        // and a lambda-only ColorSpace.Rgb has no native handle usable by Bitmap.createBitmap.
        return ColorSpace.get(if (transfer == 16) ColorSpace.Named.BT2020_PQ else ColorSpace.Named.BT2020_HLG)
    }
    platform?.let { return it }
    if (primaries == null || transfer == null) return null
    val gamut = avifPrimaries(primaries) ?: return null
    val rgb = ColorSpace.get(gamut) as ColorSpace.Rgb
    val transferSpace = when (transfer) {
        2, 13 -> ColorSpace.Named.SRGB
        1, 6 -> ColorSpace.Named.BT709
        14, 15 -> ColorSpace.Named.BT2020
        8 -> ColorSpace.Named.LINEAR_SRGB
        else -> return null
    }
    if (gamut == ColorSpace.Named.SRGB && transferSpace == ColorSpace.Named.SRGB) return rgb
    return ColorSpace.Rgb("AVIF $primaries/$transfer", rgb.primaries, rgb.whitePoint,
        (ColorSpace.get(transferSpace) as ColorSpace.Rgb).transferParameters!!)
}

private fun avifPrimaries(code: Int?): ColorSpace.Named? = when (code) {
    1, 2 -> ColorSpace.Named.SRGB
    9 -> ColorSpace.Named.BT2020
    11 -> ColorSpace.Named.DCI_P3
    12 -> ColorSpace.Named.DISPLAY_P3
    else -> null
}

internal fun orientAvifBitmap(bitmap: Bitmap, quarterTurns: Int, mirrorAxis: Int?): Bitmap {
    if (quarterTurns == 0 && mirrorAxis == null) return bitmap
    val matrix = Matrix().apply {
        postRotate(-90f * quarterTurns)
        if (mirrorAxis != null) postScale(if (mirrorAxis == 1) -1f else 1f, if (mirrorAxis == 0) -1f else 1f)
    }
    return Bitmap.createBitmap(bitmap, 0, 0, bitmap.width, bitmap.height, matrix, true).also {
        if (it !== bitmap) bitmap.recycle() // This input has never been published to a renderer.
    }
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
