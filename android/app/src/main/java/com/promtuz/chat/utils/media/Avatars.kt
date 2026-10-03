package com.promtuz.chat.utils.media

import android.util.LruCache
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.produceState
import androidx.compose.ui.graphics.ImageBitmap
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.core.CoreBridge
import com.promtuz.core.adapter.CoreEventBus
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import timber.log.Timber

/** Decoded pictures by hex IPK, misses included. A picture change empties the cache and moves
 *  [generation], which every [rememberAvatar] keys on. */
object AvatarImages {
    private val cache = LruCache<String, Any>(128)
    private val NONE = Any()
    private val cacheLock = Any()
    private val loads = Mutex()

    private val _generation = MutableStateFlow(0)
    val generation: StateFlow<Int> = _generation.asStateFlow()

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    init {
        scope.launch {
            // The doorbell rings for every message; core's generation says whether a picture changed.
            var seen = -1L
            CoreEventBus.dbChanged.collect {
                val now = runCatching { CoreBridge.avatarGeneration().toLong() }.getOrNull()
                    ?: return@collect
                if (now != seen) {
                    seen = now
                    invalidateAll()
                }
            }
        }
    }

    fun peek(ipkHex: String): ImageBitmap? = synchronized(cacheLock) {
        cache.get(ipkHex) as? ImageBitmap
    }

    suspend fun load(ipkHex: String): ImageBitmap? = withContext(Dispatchers.IO) {
        loads.withLock {
            while (true) {
                currentCoroutineContext().ensureActive()
                val startedAt = synchronized(cacheLock) {
                    when (val hit = cache.get(ipkHex)) {
                        is ImageBitmap -> return@withLock hit
                        NONE -> return@withLock null
                    }
                    _generation.value
                }
                val bytes = try {
                    if (ipkHex.startsWith("group:")) CoreBridge.groupPicture(ipkHex.removePrefix("group:").fromHex())
                    else CoreBridge.avatarOf(ipkHex.fromHex())
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    Timber.tag("Avatar").d(e, "Picture unavailable")
                    return@withLock null // A failed read is not an authoritative absence.
                }
                val image = bytes?.let { decodeAvatar(it) }
                currentCoroutineContext().ensureActive()
                synchronized(cacheLock) {
                    // Invalidation can land mid-decode; retry rather than cache the older result.
                    if (startedAt == _generation.value) {
                        cache.put(ipkHex, image ?: NONE)
                        return@withLock image
                    }
                }
            }
            @Suppress("UNREACHABLE_CODE")
            null
        }
    }

    fun invalidateAll() = synchronized(cacheLock) {
        cache.evictAll()
        _generation.value++
    }
}

@Composable
fun rememberAvatar(ipkHex: String?): ImageBitmap? {
    if (ipkHex == null) return null
    val generation by AvatarImages.generation.collectAsState()
    return key(ipkHex) {
        // Keeps the picture during a refresh, but never carries it to another person in a reused row.
        produceState(AvatarImages.peek(ipkHex), generation) {
            value = AvatarImages.load(ipkHex)
        }.value
    }
}

/** Match core's 256px avatar encoder; a small compressed file can still decode huge. */
suspend fun decodeAvatar(bytes: ByteArray): ImageBitmap? = withContext(Dispatchers.Default) {
    decodeAvif(bytes, maxEdge = 256)
}
