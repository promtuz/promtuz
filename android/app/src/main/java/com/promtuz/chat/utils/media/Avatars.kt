package com.promtuz.chat.utils.media

import android.util.LruCache
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.compose.ui.graphics.ImageBitmap
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.core.CoreBridge
import com.promtuz.core.adapter.CoreEventBus
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch

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
    fun peek(ipkHex: String): ImageBitmap? = cache.get(ipkHex) as? ImageBitmap

    /** Decode (or recall) the picture for [ipkHex]; null when they have none. */
    suspend fun load(ipkHex: String): ImageBitmap? {
        when (val hit = cache.get(ipkHex)) {
            is ImageBitmap -> return hit
            NONE -> return null
        }
        val bytes = runCatching { CoreBridge.avatarOf(ipkHex.fromHex()) }.getOrNull()
        val image = bytes?.let { decodeAvif(it, maxEdge = 1024) }
        cache.put(ipkHex, image ?: NONE)
        return image
    }

    /** Forget every decode: a picture changed somewhere, ours or theirs. */
    fun invalidateAll() {
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
    val image by produceState(AvatarImages.peek(ipkHex), ipkHex, generation) {
        value = AvatarImages.load(ipkHex)
    }
    return image
}
