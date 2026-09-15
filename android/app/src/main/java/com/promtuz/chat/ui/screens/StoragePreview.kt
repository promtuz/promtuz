package com.promtuz.chat.ui.screens

import android.graphics.Bitmap
import android.graphics.ImageDecoder
import android.os.Build
import android.util.LruCache
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.data.storage.StorageMediaItem
import com.promtuz.chat.data.storage.StorageSource
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.utils.media.decodeAvif
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.sync.Semaphore
import kotlinx.coroutines.sync.withPermit
import kotlinx.coroutines.withContext
import org.aomedia.avif.android.AvifDecoder
import java.nio.ByteBuffer

// Keep scrolling independent of image decoding. Full-size fallback decoding is
// serialized; only 96px previews survive in this bounded, screen-local cache.
internal class StoragePreviews {
    private val cache = LruCache<String, ImageBitmap>(64)
    private val decoder = Semaphore(1)

    suspend fun get(source: StorageSource, item: StorageMediaItem): ImageBitmap? = withContext(Dispatchers.IO) {
        cache.get(item.key)?.let { return@withContext it }
        decoder.withPermit {
            cache.get(item.key)?.let { return@withPermit it }
            val bytes = source.preview(item) ?: return@withPermit null
            val bitmap = decode(bytes) ?: return@withPermit null
            bitmap.also { cache.put(item.key, it) }
        }
    }

    private fun decode(bytes: ByteArray): ImageBitmap? {
        if (Build.VERSION.SDK_INT >= 28) runCatching {
            ImageDecoder.decodeBitmap(ImageDecoder.createSource(ByteBuffer.wrap(bytes))) { decoder, info, _ ->
                val scale = 96f / maxOf(info.size.width, info.size.height).coerceAtLeast(96)
                decoder.setTargetSize((info.size.width * scale).toInt().coerceAtLeast(1),
                    (info.size.height * scale).toInt().coerceAtLeast(1))
            }.asImageBitmap()
        }.getOrNull()?.let { return it }
        val buffer = ByteBuffer.allocateDirect(bytes.size).put(bytes).apply { rewind() }
        val info = AvifDecoder.Info()
        if (!AvifDecoder.getInfo(buffer, bytes.size, info) || info.width.toLong() * info.height > 16_000_000) return null
        val full = decodeAvif(bytes)?.asAndroidBitmap() ?: return null
        val scale = 96f / maxOf(full.width, full.height).coerceAtLeast(96)
        val small = Bitmap.createScaledBitmap(full, (full.width * scale).toInt().coerceAtLeast(1),
            (full.height * scale).toInt().coerceAtLeast(1), true)
        if (small !== full) full.recycle()
        return small.asImageBitmap()
    }
}

@Composable
internal fun StorageMediaPreview(source: StorageSource, previews: StoragePreviews, item: StorageMediaItem) {
    val bitmap by produceState<ImageBitmap?>(null, source, item.key) {
        if (item.kind != 3) try { value = previews.get(source, item) }
        catch (e: CancellationException) { throw e }
        catch (_: Exception) { /* The metadata row remains usable when a preview is unavailable. */ }
    }
    Box(Modifier.size(44.dp).clip(RoundedCornerShape(12.dp)).background(MaterialTheme.colorScheme.secondaryContainer),
        contentAlignment = Alignment.Center) {
        bitmap?.let { Image(it, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop) }
            ?: DrawableIcon(when (item.kind) { 1 -> R.drawable.oi_image; 3 -> R.drawable.i_mic; else -> R.drawable.oi_paperclip }, size = 22.dp)
    }
}
