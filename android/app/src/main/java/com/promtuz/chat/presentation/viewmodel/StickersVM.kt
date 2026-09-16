package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.net.Uri
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.domain.model.StickerRef
import com.promtuz.chat.domain.model.UiStickerPack
import com.promtuz.chat.domain.model.toRecord
import com.promtuz.chat.domain.model.toRef
import com.promtuz.chat.domain.model.toUi
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.media.decodeDownscaled
import com.promtuz.chat.utils.media.toRgba
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.core.StickerSource

/** Preview of an installed or received pack. */
@Immutable
data class UiPackPreview(
    val packHex: String,
    val name: String,
    val installed: Boolean,
    val mine: Boolean,
    val stickers: List<StickerRef>,
)

/** Selected image and its thumbnail. */
@Immutable
data class PickedSticker(val uri: Uri, val preview: ImageBitmap?)

/** Installed packs and publication state, scoped to the navigation entry. */
class StickersVM(private val application: Application) : ViewModel() {
    val packs: StateFlow<List<UiStickerPack>> =
        observeQuery(setOf("sticker_packs", "stickers")) { CoreBridge.stickerPacks().map { it.toUi() } }
            .stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), emptyList())

    val recents: StateFlow<List<StickerRef>> =
        observeQuery(setOf("sticker_recents", "sticker_packs", "stickers")) {
            CoreBridge.recentStickers(RECENT_LIMIT).map { it.toRef() }
        }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), emptyList())

    /** Core rate-limits manifest refreshes per pack. */
    fun refresh() = viewModelScope.launch { runCatching { CoreBridge.refreshStickerPacks() } }

    /** Installed packs can be previewed offline. */
    suspend fun preview(ref: StickerRef): Result<UiPackPreview> {
        packs.value.firstOrNull { it.packHex == ref.packHex }?.let {
            return Result.success(UiPackPreview(it.packHex, it.name, installed = true, mine = it.mine, stickers = it.stickers))
        }
        return result {
            val p = CoreBridge.stickerPackPreview(ref.toRecord())
            UiPackPreview(ref.packHex, p.name, p.installed, p.mine, p.stickers.map { it.toRef() })
        }
    }

    suspend fun install(ref: StickerRef): Result<Unit> = result { CoreBridge.installStickerPack(ref.toRecord()) }

    suspend fun remove(packHex: String): Result<Unit> = result { CoreBridge.removeStickerPack(packHex.fromHex()) }

    var draftName by mutableStateOf("")
    val picks = mutableStateListOf<PickedSticker>()
    var decoding by mutableStateOf(false)
        private set
    var publishing by mutableStateOf(false)
        private set
    var published by mutableStateOf(false)
        private set
    var error by mutableStateOf<String?>(null)
        private set

    fun pickImages(uris: List<Uri>, room: Int) {
        if (decoding || publishing) return
        val known = picks.map { it.uri }.toSet()
        val fresh = uris.distinct().filter { it !in known }.take((room - picks.size).coerceAtLeast(0))
        if (fresh.isEmpty()) return
        decoding = true
        error = null
        viewModelScope.launch {
            try {
                val decoded = fresh.map { uri ->
                    val image = decodeDownscaled(application, uri, TILE_MAX_EDGE) ?: throw UnreadableImage()
                    PickedSticker(uri, image.asImageBitmap())
                }
                picks += decoded
            } catch (e: CancellationException) { throw e }
            catch (_: Exception) { error = "Couldn’t open the selected images. Try choosing them again." }
            finally { decoding = false }
        }
    }

    fun publish(packHex: String?) {
        if (publishing || decoding || picks.isEmpty()) return
        publishing = true
        error = null
        val uris = picks.map { it.uri }
        val name = draftName.trim()
        viewModelScope.launch {
            try {
                val images = sources(uris)
                if (packHex == null) CoreBridge.createStickerPack(name, images)
                else CoreBridge.addToStickerPack(packHex.fromHex(), images)
                published = true
            } catch (e: CancellationException) { throw e }
            catch (e: Exception) {
                error = when (e) {
                    is uniffi.core.CoreException.Refused -> e.msg
                    is UnreadableImage -> "Couldn’t open an image. Remove it and try again."
                    else -> "Couldn’t publish. Check your connection and try again."
                }
            } finally { publishing = false }
        }
    }

    fun consumePublished() { published = false }

    private class UnreadableImage : Exception()

    private suspend fun sources(uris: List<Uri>): List<StickerSource> = withContext(Dispatchers.IO) {
        uris.map { uri ->
            val bmp = decodeDownscaled(application, uri, STICKER_EDGE) ?: throw UnreadableImage()
            try { StickerSource(bmp.toRgba(), bmp.width.toUInt(), bmp.height.toUInt()) }
            finally { bmp.recycle() }
        }
    }

    private companion object {
        const val RECENT_LIMIT = 24
        /** libcore fits the picture inside this; decoding larger is wasted work. */
        const val STICKER_EDGE = 512
        const val TILE_MAX_EDGE = 192
    }
}

private suspend fun <T> result(block: suspend () -> T): Result<T> = try {
    Result.success(block())
} catch (e: CancellationException) { throw e }
catch (e: Exception) { Result.failure(e) }
