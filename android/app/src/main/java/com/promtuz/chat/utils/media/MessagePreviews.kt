package com.promtuz.chat.utils.media

import android.graphics.Bitmap
import android.util.LruCache
import androidx.compose.runtime.*
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import com.promtuz.chat.domain.model.toRef
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.core.CoreBridge
import com.promtuz.core.adapter.CoreEventBus
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.sync.Semaphore
import kotlinx.coroutines.sync.withPermit
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull

/** Small decoded previews; authoritative attachments stay owned by core. */
object MessagePreviews {
    private val decoders = Semaphore(2)
    private val cache = object : LruCache<String, Bitmap>(4 * 1024 * 1024) {
        override fun sizeOf(key: String, value: Bitmap) = value.allocationByteCount
    }

    suspend fun load(conversation: String, dispatch: String): Bitmap? = withContext(Dispatchers.IO) {
        try {
            val media = CoreBridge.getMessageMedia(conversation.fromHex(), dispatch.fromHex()) ?: return@withContext null
            if (media.kind.toInt() == 4) {
                val ref = media.sticker?.toRef() ?: return@withContext null
                return@withContext withTimeoutOrNull(2_000) { StickerImages.load(ref)?.asAndroidBitmap()?.tile() }
            }
            val bytes = when {
                media.kind.toInt() == 1 -> media.blob
                media.mime.startsWith("image/") || media.mime.startsWith("video/") -> media.thumb
                else -> null
            } ?: return@withContext null
            val key = "$conversation:$dispatch:${bytes.contentHashCode()}"
            cache.get(key) ?: decoders.withPermit {
                cache.get(key) ?: decodeAvif(bytes, maxEdge = 4096)?.asAndroidBitmap()?.tile()
                    ?.also { cache.put(key, it) }
            }
        } catch (e: CancellationException) { throw e }
        catch (_: Exception) { null }
    }

    private fun Bitmap.tile(): Bitmap {
        val scale = minOf(1f, 192f / maxOf(width, height))
        return Bitmap.createScaledBitmap(this, (width * scale).toInt().coerceAtLeast(1),
            (height * scale).toInt().coerceAtLeast(1), true)
    }
}

@Composable
fun rememberMessagePreview(conversation: String, dispatch: String, retry: Boolean = false) = run {
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    produceState<androidx.compose.ui.graphics.ImageBitmap?>(null, conversation, dispatch, lifecycle) {
        value = null
        lifecycle.repeatOnLifecycle(Lifecycle.State.STARTED) {
            merge(
                CoreEventBus.dbChanged.filter { tables -> tables.any { it in setOf("messages", "message_media", "partials") } }.map { Unit },
                CoreBridge.connection.map { Unit },
            ).collectLatest {
                do {
                    value = MessagePreviews.load(conversation, dispatch)?.asImageBitmap()
                    if (value == null && retry) delay(10_000)
                } while (value == null && retry)
            }
        }
    }.value
}
