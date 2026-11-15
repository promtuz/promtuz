package com.promtuz.chat.ui.screens

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
import java.nio.ByteBuffer

// Keep scrolling independent of image decoding. Full-size fallback decoding is
// serialized; only 240px previews survive in this bounded, screen-local cache.
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
                val scale = 240f / maxOf(info.size.width, info.size.height).coerceAtLeast(240)
                decoder.setTargetSize((info.size.width * scale).toInt().coerceAtLeast(1),
                    (info.size.height * scale).toInt().coerceAtLeast(1))
            }.asImageBitmap()
        }.getOrNull()?.let { return it }
        return decodeAvif(bytes, 240)
    }
}
