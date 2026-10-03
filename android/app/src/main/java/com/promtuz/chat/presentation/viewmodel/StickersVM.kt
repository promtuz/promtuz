package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.net.Uri
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.ImageBitmap
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.domain.model.StickerRef
import com.promtuz.chat.domain.model.UiStickerPack
import com.promtuz.chat.domain.model.toRecord
import com.promtuz.chat.domain.model.toRef
import com.promtuz.chat.domain.model.toUi
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.media.pickImageSource
import com.promtuz.chat.utils.media.ImagePreparationException
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

@Immutable
data class UiPackPreview(
    val packHex: String,
    val name: String,
    val installed: Boolean,
    val mine: Boolean,
    val stickers: List<StickerRef>,
)

@Immutable
data class PickedSticker(val uri: Uri, val preview: ImageBitmap?, val encoded: ByteArray? = null)

class StickersVM(private val application: Application) : ViewModel() {
    val packs: StateFlow<List<UiStickerPack>> =
        observeQuery(setOf("sticker_packs", "stickers")) { CoreBridge.stickerPacks().map { it.toUi() } }
            .stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), emptyList())

    val recents: StateFlow<List<StickerRef>> =
        observeQuery(setOf("sticker_recents", "sticker_packs", "stickers")) {
            CoreBridge.recentStickers(RECENT_LIMIT).map { it.toRef() }
        }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), emptyList())

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
                    val image = pickImageSource(application, uri, STICKER_EDGE, sticker = true)
                    if (image.preserveOriginal) throw ImagePreparationException("Use an AVIF to keep this image’s HDR or colors in a sticker.")
                    PickedSticker(uri, image.preview, image.encoded?.takeIf { it.size <= 1024 * 1024 })
                }
                picks += decoded
            } catch (e: CancellationException) { throw e }
            catch (e: Exception) { error = (e as? ImagePreparationException)?.message ?: "Couldn’t open the selected images. Try choosing them again." }
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
                    is ImagePreparationException -> e.message
                    else -> "Couldn’t publish. Check your connection and try again."
                }
            } finally { publishing = false }
        }
    }

    fun consumePublished() { published = false }

    private class UnreadableImage : Exception()

    private suspend fun sources(uris: List<Uri>): List<StickerSource> = withContext(Dispatchers.IO) {
        uris.map { uri ->
            val image = pickImageSource(application, uri, STICKER_EDGE, sticker = true)
            if (image.preserveOriginal) throw ImagePreparationException("Use an AVIF to keep this image’s HDR or colors in a sticker.")
            val prepared = image.encoded?.let { CoreBridge.prepareEncodedImage(it, sticker = true) }
            StickerSource(image.rgba ?: ByteArray(0), image.width.toUInt(), image.height.toUInt(), prepared?.bytes)
        }
    }

    private companion object {
        const val RECENT_LIMIT = 24
        /** libcore fits the picture inside this; decoding larger is wasted work. */
        const val STICKER_EDGE = 512
    }
}

private suspend fun <T> result(block: suspend () -> T): Result<T> = try {
    Result.success(block())
} catch (e: CancellationException) { throw e }
catch (e: Exception) { Result.failure(e) }
