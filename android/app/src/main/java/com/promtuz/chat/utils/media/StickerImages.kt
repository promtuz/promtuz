package com.promtuz.chat.utils.media

import android.util.LruCache
import androidx.compose.runtime.Composable
import androidx.compose.runtime.produceState
import androidx.compose.ui.graphics.ImageBitmap
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import com.promtuz.chat.domain.model.StickerRef
import com.promtuz.chat.domain.model.toRecord
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.sync.Semaphore
import kotlinx.coroutines.sync.withPermit
import kotlinx.coroutines.withContext
import timber.log.Timber

/** Decoded images shared by chat bubbles, pack covers and picker cells. */
object StickerImages {
    private val cache = object : LruCache<String, ImageBitmap>(64 * 1024 * 1024) {
        override fun sizeOf(key: String, value: ImageBitmap) = value.width * value.height * 4
    }

    fun peek(ref: StickerRef): ImageBitmap? = cache.get(ref.key)

    fun clear() = cache.evictAll()

    private val decoders = Semaphore(2)

    suspend fun load(ref: StickerRef): ImageBitmap? = peek(ref) ?: withContext(Dispatchers.IO) {
        try {
            val bytes = CoreBridge.stickerImage(ref.toRecord())
            decoders.withPermit {
                peek(ref) ?: decodeAvif(bytes, maxEdge = 512)?.also { cache.put(ref.key, it) }
            }
        } catch (e: CancellationException) { throw e }
        catch (e: Exception) {
            Timber.tag("Stickers").d(e, "Image unavailable: %s", ref.key)
            null
        }
    }
}

@Composable
fun rememberStickerBitmap(ref: StickerRef): ImageBitmap? {
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    return produceState(StickerImages.peek(ref), ref.key, lifecycle) {
        value = StickerImages.peek(ref)
        lifecycle.repeatOnLifecycle(Lifecycle.State.STARTED) {
            CoreBridge.connection.collectLatest {
                var retryMs = 1_000L
                while (value == null) {
                    value = StickerImages.load(ref)
                    if (value == null) {
                        delay(retryMs)
                        retryMs = (retryMs * 2).coerceAtMost(30_000L)
                    }
                }
            }
        }
    }.value
}
