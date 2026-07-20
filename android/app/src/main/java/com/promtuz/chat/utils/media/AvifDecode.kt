package com.promtuz.chat.utils.media

import android.graphics.ImageDecoder
import android.os.Build
import android.util.LruCache
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import java.nio.ByteBuffer

/**
 * Decode an inline/preview AVIF blob. ImageDecoder gained native AVIF on API 31
 * (S); on 26–30 there's no decoder so this returns null and the bubble draws a
 * placeholder — a known v1 gap, never a crash.
 */
fun decodeAvif(bytes: ByteArray): ImageBitmap? {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return null
    return runCatching {
        ImageDecoder.decodeBitmap(ImageDecoder.createSource(ByteBuffer.wrap(bytes))).asImageBitmap()
    }.getOrNull()
}

// Keyed by dispatch-id-hex so re-reads on every DB doorbell hand back the SAME
// ImageBitmap instance, keeping the @Immutable content's value-equality stable
// (a fresh decode each tick would churn MessageStage).
private val cache = LruCache<String, ImageBitmap>(64)

fun decodeAvifCached(dispatchIdHex: String, bytes: ByteArray): ImageBitmap? =
    cache.get(dispatchIdHex) ?: decodeAvif(bytes)?.also { cache.put(dispatchIdHex, it) }
