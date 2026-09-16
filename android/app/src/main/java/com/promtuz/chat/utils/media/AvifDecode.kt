package com.promtuz.chat.utils.media

import android.graphics.Bitmap
import android.graphics.ImageDecoder
import android.os.Build
import android.util.LruCache
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import org.aomedia.avif.android.AvifDecoder
import timber.log.Timber
import java.nio.ByteBuffer

/**
 * Decode an inline/preview AVIF blob. Bundled libavif (dav1d) decodes first —
 * the platform ImageDecoder is device-specific: absent on API 26–30, and some
 * 31+ builds reject our encoder's output → null bitmap → grey bubble. The
 * platform path stays as a fallback. Every failure branch logs so a grey
 * bubble is diagnosable.
 */
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
    // libavif's JNI reads via GetDirectBufferAddress — a wrapped array won't do.
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
    // UnsatisfiedLinkError and friends — fall through to the platform decoder.
    Timber.tag("Avif").w(it, "libavif threw")
}.getOrNull()

// Keyed by dispatch-id-hex so re-reads on every DB doorbell hand back the SAME
// ImageBitmap instance, keeping the @Immutable content's value-equality stable
// (a fresh decode each tick would churn MessageStage).
private val cache = LruCache<String, ImageBitmap>(64)

fun decodeAvifCached(dispatchIdHex: String, bytes: ByteArray): ImageBitmap? =
    cache.get(dispatchIdHex) ?: decodeAvif(bytes)?.also { cache.put(dispatchIdHex, it) }
