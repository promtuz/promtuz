package com.promtuz.chat.utils.media

import android.graphics.Bitmap
import android.util.LruCache
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import com.promtuz.core.CoreBridge

/** A static poster. [maxEdge] validates the source dimensions, as required by avatars/stickers. */
fun decodeAvif(bytes: ByteArray, maxEdge: Int = Int.MAX_VALUE): ImageBitmap? =
    decodeCoreAvif(bytes, maxEdge.coerceAtMost(4096))

// Re-reads get the same instance, so immutable message content stays equal. A revision can
// replace the picture under the same dispatch id, hence the content hash in the key.
private val cache = object : LruCache<String, ImageBitmap>(64 shl 20) {
    override fun sizeOf(key: String, value: ImageBitmap) = imageAllocationBytes(value)
}

fun decodeAvifCached(dispatchIdHex: String, bytes: ByteArray): ImageBitmap? {
    val key = "$dispatchIdHex:${bytes.contentHashCode()}"
    return cache.get(key) ?: decodeAvif(bytes)?.also { cache.put(key, it) }
}

/** Decode a bounded poster directly, rather than allocating a full-resolution image first. */
fun decodeAvifThumb(bytes: ByteArray, edge: Int): ImageBitmap? = decodeEncodedPoster(bytes, edge)

/** The output edge is capped here; larger valid source images are downsampled. Call on a worker. */
fun decodeEncodedPoster(bytes: ByteArray, maxEdge: Int): ImageBitmap? =
    decodeCoreAvif(bytes, maxEdge)

internal fun decodeCoreAvif(bytes: ByteArray, maxEdge: Int): ImageBitmap? = runCatching {
    val frame = CoreBridge.decodeAvifSdr(bytes, maxEdge.coerceAtLeast(1).toUInt())
    bitmapFromRgba(frame.pixels, frame.width.toInt(), frame.height.toInt())
}.getOrNull()

internal fun bitmapFromRgba(pixels: ByteArray, width: Int, height: Int): ImageBitmap {
    require(pixels.size == width * height * 4)
    val argb = IntArray(width * height) { index ->
        val offset = index * 4
        ((pixels[offset + 3].toInt() and 0xff) shl 24) or
            ((pixels[offset].toInt() and 0xff) shl 16) or
            ((pixels[offset + 1].toInt() and 0xff) shl 8) or
            (pixels[offset + 2].toInt() and 0xff)
    }
    return Bitmap.createBitmap(argb, width, height, Bitmap.Config.ARGB_8888).asImageBitmap()
}
