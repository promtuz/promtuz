package com.promtuz.chat.ui.text

import android.content.Context
import android.graphics.BitmapFactory
import android.util.LruCache
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import timber.log.Timber

/**
 * The bundled emoji set under `assets/emoji/`: an index of the keys it holds
 * (see `tools/scripts/build-emoji-pack.py`) and a cache of decoded glyphs.
 *
 * The index loads once, off the main thread; until it has, [EmojiText] draws
 * system emoji, then redraws from the pack. Glyphs decode on demand and stay
 * in a bounded cache, so a long chat re-uses the same few dozen bitmaps.
 */
object EmojiPack {
    private const val DIR = "emoji"

    class Index(val keys: Set<String>, val aliases: Map<String, String>)

    private val _index = MutableStateFlow<Index?>(null)
    val index: StateFlow<Index?> = _index

    private val glyphs = LruCache<String, ImageBitmap>(512)
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    @Volatile private var loading = false

    /** Start loading the index if nobody has; safe to call from every composition. */
    fun ensureLoaded(context: Context) {
        if (_index.value != null || loading) return
        loading = true
        val app = context.applicationContext
        scope.launch {
            _index.value = runCatching { load(app) }
                .onFailure { Timber.tag("Emoji").w(it, "pack index missing; system emoji stay") }
                .getOrNull() ?: Index(emptySet(), emptyMap())
        }
    }

    private fun load(context: Context): Index {
        val keys = HashSet<String>(4096)
        val aliases = HashMap<String, String>()
        context.assets.open("$DIR/index.txt").bufferedReader().useLines { lines ->
            for (line in lines) {
                val sep = line.indexOf('>')
                if (sep < 0) keys += line else aliases[line.substring(0, sep)] = line.substring(sep + 1)
            }
        }
        return Index(keys, aliases)
    }

    fun peek(key: String): ImageBitmap? = glyphs.get(key)

    /** Decode (or recall) the glyph for [key]; null when the asset is missing or unreadable. */
    suspend fun glyph(context: Context, key: String): ImageBitmap? {
        glyphs.get(key)?.let { return it }
        val app = context.applicationContext
        return withContext(Dispatchers.IO) {
            runCatching {
                app.assets.open("$DIR/$key.webp").use { BitmapFactory.decodeStream(it) }?.asImageBitmap()
            }.onFailure { Timber.tag("Emoji").w(it, "glyph $key failed to decode") }
                .getOrNull()
                ?.also { glyphs.put(key, it) }
        }
    }
}
