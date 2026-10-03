package com.promtuz.chat.utils.media

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
import kotlinx.coroutines.sync.withPermit
import kotlinx.coroutines.withContext
import timber.log.Timber

private const val AVATAR_EDGE = 256
private const val MAX_AVATAR_BYTES = 64 * 1024

/** Original AVIF and its reusable display source. Treat [encoded] as immutable. */
class AvatarPicture internal constructor(
    val encoded: ByteArray,
    internal val prepared: PreparedEncodedImage,
) {
    val poster: ImageBitmap get() = prepared.poster
    val hdr: Boolean get() = prepared.hdr

    // The animation's direct buffer is a separate allocation from the retained original.
    internal val allocationBytes: Long = encoded.size.toLong() + prepared.allocationBytes
}

/** Pictures by hex IPK, misses included. A picture change empties the cache and moves
 *  [generation], which every [rememberAvatar] keys on. */
object AvatarImages {
    private const val MAX_CACHE_ENTRIES = 128
    private const val MAX_CACHE_BYTES = 16 * 1024 * 1024L
    private class Entry(val picture: AvatarPicture?) {
        val allocationBytes: Long = picture?.allocationBytes ?: 0L
    }

    private val cache = LinkedHashMap<String, Entry>(16, 0.75f, true)
    private var cacheBytes = 0L
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

    fun peekPicture(ipkHex: String): AvatarPicture? = synchronized(cacheLock) {
        cache[ipkHex]?.picture
    }

    /** Static consumers such as notifications use the same cache and keep a poster-only API. */
    fun peek(ipkHex: String): ImageBitmap? = peekPicture(ipkHex)?.poster

    suspend fun load(ipkHex: String): ImageBitmap? = loadPicture(ipkHex)?.poster

    suspend fun loadPicture(ipkHex: String): AvatarPicture? = withContext(Dispatchers.IO) {
        loads.withLock {
            while (true) {
                currentCoroutineContext().ensureActive()
                val startedAt = synchronized(cacheLock) {
                    cache[ipkHex]?.let { return@withLock it.picture }
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
                val image = bytes?.let { prepareAvatar(it) }
                currentCoroutineContext().ensureActive()
                synchronized(cacheLock) {
                    // Invalidation can land mid-decode; retry rather than cache the older result.
                    if (startedAt == _generation.value) {
                        val entry = Entry(image)
                        cache.put(ipkHex, entry)?.let { cacheBytes -= it.allocationBytes }
                        cacheBytes += entry.allocationBytes
                        val oldest = cache.entries.iterator()
                        while (cache.size > MAX_CACHE_ENTRIES || cacheBytes > MAX_CACHE_BYTES) {
                            cacheBytes -= oldest.next().value.allocationBytes
                            oldest.remove()
                        }
                        // Do not recycle evicted posters: visible rows and notifications may
                        // still own them. Prepared sources never retain a native decoder.
                        return@withLock image
                    }
                }
            }
            @Suppress("UNREACHABLE_CODE")
            null
        }
    }

    fun invalidateAll() = synchronized(cacheLock) {
        cache.clear()
        cacheBytes = 0L
        _generation.value++
    }
}

@Composable
fun rememberAvatar(ipkHex: String?): AvatarPicture? {
    if (ipkHex == null) return null
    val generation by AvatarImages.generation.collectAsState()
    return key(ipkHex) {
        // Keeps the picture during a refresh, but never carries it to another person in a reused row.
        produceState(AvatarImages.peekPicture(ipkHex), generation) {
            value = AvatarImages.loadPicture(ipkHex)
        }.value
    }
}

/** Match core's avatar limits; a small compressed file can still declare huge coded frames. */
suspend fun prepareAvatar(bytes: ByteArray): AvatarPicture? = withContext(Dispatchers.Default) {
    if (bytes.isEmpty() || bytes.size > MAX_AVATAR_BYTES) return@withContext null
    imageDecodeWork.withPermit {
        // Own the original independently of the caller and retain it for viewing/export.
        val original = bytes.copyOf()
        val prepared = prepareEncodedImage(original, sourceMaxEdge = AVATAR_EDGE, targetEdge = AVATAR_EDGE)
            ?: return@withPermit null
        AvatarPicture(original, prepared)
    }
}

suspend fun decodeAvatar(bytes: ByteArray): ImageBitmap? = prepareAvatar(bytes)?.poster
