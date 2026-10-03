package com.promtuz.chat.utils.media

import android.graphics.Bitmap
import android.graphics.ImageDecoder
import android.os.Build
import android.util.LruCache
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.graphics.asImageBitmap
import org.aomedia.avif.android.AvifDecoder
import timber.log.Timber
import java.nio.ByteBuffer

/** Bundled libavif goes first: the platform decoder is missing below API 31 and some builds reject our output. */
fun decodeAvif(bytes: ByteArray, maxEdge: Int = Int.MAX_VALUE): ImageBitmap? {
    if (bytes.isEmpty()) {
        Timber.tag("Avif").w("decode skipped: empty blob")
        return null
    }
    decodeWithLibavif(bytes, maxEdge)?.let { return it }
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return null
    return runCatching {
        ImageDecoder.decodeBitmap(ImageDecoder.createSource(ByteBuffer.wrap(bytes))) { _, info, _ ->
            require(info.size.width in 1..maxEdge && info.size.height in 1..maxEdge) { "Image dimensions exceed limit" }
        }.asImageBitmap()
    }.onFailure {
        Timber.tag("Avif").w(it, "platform decode failed: ${bytes.size}B, sdk ${Build.VERSION.SDK_INT}")
    }.getOrNull()
}

private fun decodeWithLibavif(bytes: ByteArray, maxEdge: Int): ImageBitmap? = runCatching {
    // libavif's JNI reads via GetDirectBufferAddress, so a wrapped array won't do.
    val buf = ByteBuffer.allocateDirect(bytes.size).put(bytes).apply { rewind() }
    val info = AvifDecoder.Info()
    if (!AvifDecoder.getInfo(buf, bytes.size, info)) {
        Timber.tag("Avif").w("libavif getInfo failed: ${bytes.size}B")
        return@runCatching null
    }
    if (info.width !in 1..maxEdge || info.height !in 1..maxEdge) return@runCatching null
    val bitmap = Bitmap.createBitmap(info.width, info.height, Bitmap.Config.ARGB_8888)
    if (!AvifDecoder.decode(buf, bytes.size, bitmap)) {
        Timber.tag("Avif").w("libavif decode failed: ${info.width}x${info.height}, ${bytes.size}B")
        return@runCatching null
    }
    bitmap.asImageBitmap()
}.onFailure {
    // UnsatisfiedLinkError and the like fall through to the platform decoder.
    Timber.tag("Avif").w(it, "libavif threw")
}.getOrNull()

// Re-reads get the same instance, so @Immutable content stays equal. The bytes are in the key
// because a revision replaces the picture under the same dispatch id.
private val cache = object : LruCache<String, ImageBitmap>(64 shl 20) {
    override fun sizeOf(key: String, value: ImageBitmap) = value.width * value.height * 4
}

fun decodeAvifCached(dispatchIdHex: String, bytes: ByteArray): ImageBitmap? {
    val key = "$dispatchIdHex:${bytes.contentHashCode()}"
    return cache.get(key) ?: decodeAvif(bytes)?.also { cache.put(key, it) }
}

fun decodeAvifThumb(bytes: ByteArray, edge: Int): ImageBitmap? {
    val full = decodeAvif(bytes)?.asAndroidBitmap() ?: return null
    val scale = minOf(1f, edge.toFloat() / maxOf(full.width, full.height))
    if (scale == 1f) return full.asImageBitmap()
    val small = Bitmap.createScaledBitmap(full, (full.width * scale).toInt().coerceAtLeast(1),
        (full.height * scale).toInt().coerceAtLeast(1), true)
    full.recycle()
    return small.asImageBitmap()
}
