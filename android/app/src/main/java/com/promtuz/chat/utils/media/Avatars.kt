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

/**
 * Decoded profile pictures, keyed by hex IPK.
 *
 * One decode per person rather than one per row: the home list, the chat header
 * and a member list all ask for the same face. A miss is remembered too, so a
 * person with no picture costs one core call, not one per recomposition. When a
 * picture changes, ours or theirs, the cache empties and [generation] moves,
 * which every [rememberAvatar] keys on, so the change redraws wherever it is
 * shown.
 */
object AvatarImages {
    /** Hex IPK → decoded picture, or [NONE] for someone known to have none. */
    private val cache = LruCache<String, Any>(128)
    private val NONE = Any()
    private val cacheLock = Any()
    private val loads = Mutex()

    private val _generation = MutableStateFlow(0)
    val generation: StateFlow<Int> = _generation.asStateFlow()

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    init {
        scope.launch {
            // The doorbell rings for every commit in the messages DB, which is
            // every message. Core's generation says whether a picture was among
            // it, at the cost of one call rather than a decode per face.
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

    /** What is already decoded for [ipkHex], for a first frame without a flash. */
    fun peek(ipkHex: String): ImageBitmap? = synchronized(cacheLock) {
        cache.get(ipkHex) as? ImageBitmap
    }

    /** Reads and decoding stay off main. Concurrent callers share the cached result. */
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
                    CoreBridge.avatarOf(ipkHex.fromHex())
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    Timber.tag("Avatar").d(e, "Picture unavailable")
                    return@withLock null // A failed read is not an authoritative absence.
                }
                val image = bytes?.let { decodeAvatar(it) }
                currentCoroutineContext().ensureActive()
                synchronized(cacheLock) {
                    // Invalidation can arrive during the read or decode. Retry
                    // instead of repopulating the cache with the older result.
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

    /** Forget decoded images and advance their generation as one operation. */
    fun invalidateAll() = synchronized(cacheLock) {
        cache.evictAll()
        _generation.value++
    }
}

/**
 * The picture for [ipkHex], for an [com.promtuz.chat.ui.components.Avatar] to
 * draw. Null while unknown or absent, which draws initials; re-resolved when
 * the cache generation moves.
 */
@Composable
fun rememberAvatar(ipkHex: String?): ImageBitmap? {
    if (ipkHex == null) return null
    val generation by AvatarImages.generation.collectAsState()
    return key(ipkHex) {
        // Keep the current picture during refresh, but never carry it to a
        // different person when a list row's identity changes.
        produceState(AvatarImages.peek(ipkHex), generation) {
            value = AvatarImages.load(ipkHex)
        }.value
    }
}

/** Match core's 256px avatar encoder; a small compressed file can still decode huge. */
suspend fun decodeAvatar(bytes: ByteArray): ImageBitmap? = withContext(Dispatchers.Default) {
    decodeAvif(bytes, maxEdge = 256)
}
