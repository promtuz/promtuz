package com.promtuz.chat.utils.media

import android.util.LruCache
import androidx.compose.foundation.background
import androidx.compose.runtime.Composable
import androidx.compose.runtime.produceState
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.ContentScale
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
import kotlinx.coroutines.sync.withPermit
import kotlinx.coroutines.withContext
import timber.log.Timber

object StickerImages {
    private val cache = object : LruCache<String, PreparedEncodedImage>(64 * 1024 * 1024) {
        override fun sizeOf(key: String, value: PreparedEncodedImage) = value.allocationBytes
    }

    fun peek(ref: StickerRef): ImageBitmap? = peekPrepared(ref)?.poster

    internal fun peekPrepared(ref: StickerRef): PreparedEncodedImage? = cache.get(ref.key)

    fun clear() = cache.evictAll()

    suspend fun load(ref: StickerRef): ImageBitmap? = loadPrepared(ref)?.poster

    internal suspend fun loadPrepared(ref: StickerRef): PreparedEncodedImage? = peekPrepared(ref) ?: withContext(Dispatchers.IO) {
        try {
            val bytes = CoreBridge.stickerImage(ref.toRecord())
            withContext(Dispatchers.Default) {
                imageDecodeWork.withPermit {
                    peekPrepared(ref) ?: prepareEncodedImage(bytes, sourceMaxEdge = 512, targetEdge = 512)
                        ?.also { cache.put(ref.key, it) }
                }
            }
        } catch (e: CancellationException) { throw e }
        catch (e: Exception) {
            Timber.tag("Stickers").d(e, "Image unavailable: %s", ref.key)
            null
        }
    }
}

@Composable
fun rememberStickerBitmap(ref: StickerRef): ImageBitmap? = rememberPreparedSticker(ref)?.poster

@Composable
fun StickerImage(
    ref: StickerRef,
    contentDescription: String?,
    modifier: Modifier = Modifier,
    poster: ImageBitmap? = null,
    contentScale: ContentScale = ContentScale.Fit,
    animate: Boolean = true,
    placeholderColor: Color? = null,
) {
    val prepared = rememberPreparedSticker(ref)
    val imageModifier = if (prepared == null && poster == null && placeholderColor != null) {
        modifier.background(placeholderColor)
    } else modifier
    EncodedImageContent(prepared, contentDescription, imageModifier, poster, contentScale, animate)
}

@Composable
private fun rememberPreparedSticker(ref: StickerRef): PreparedEncodedImage? {
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    return produceState(StickerImages.peekPrepared(ref), ref.key, lifecycle) {
        value = StickerImages.peekPrepared(ref)
        lifecycle.repeatOnLifecycle(Lifecycle.State.STARTED) {
            CoreBridge.connection.collectLatest {
                var retryMs = 1_000L
                while (value == null) {
                    value = StickerImages.loadPrepared(ref)
                    if (value == null) {
                        delay(retryMs)
                        retryMs = (retryMs * 2).coerceAtMost(30_000L)
                    }
                }
            }
        }
    }.value
}
