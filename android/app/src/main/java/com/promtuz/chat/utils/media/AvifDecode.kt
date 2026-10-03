package com.promtuz.chat.utils.media

import android.util.LruCache
import androidx.compose.ui.graphics.ImageBitmap

/** A static poster. [maxEdge] validates the source dimensions, as required by avatars/stickers. */
fun decodeAvif(bytes: ByteArray, maxEdge: Int = Int.MAX_VALUE): ImageBitmap? =
    prepareEncodedImage(bytes, sourceMaxEdge = maxEdge, targetEdge = 4096, animation = false)?.poster

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
    prepareEncodedImage(bytes, targetEdge = maxEdge, animation = false)?.poster
