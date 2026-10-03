package com.promtuz.chat.ui.media

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.node.GlobalPositionAwareModifierNode
import androidx.compose.ui.node.CompositionLocalConsumerModifierNode
import androidx.compose.ui.node.currentValueOf
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.ui.node.ModifierNodeElement
import androidx.compose.ui.platform.InspectorInfo
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.geometry.Size
import com.promtuz.chat.ui.components.MenuAction

/** [thumb] is a fallback preview; [encoded] or [filePath] retains the original image for playback and export. */
data class MediaItem(
    val key: String,
    val thumb: ImageBitmap?,
    val width: Int,
    val height: Int,
    val title: String = "",
    val subtitle: String = "",
    val caption: String = "",
    val videoPath: String? = null,
    val mime: String = "image/jpeg",
    val filePath: String? = null,
    /** Items of one album share a group, which is what the filmstrip shows. */
    val group: String? = null,
    val load: suspend () -> ImageBitmap? = { thumb },
    val shareName: String = key,
    val byteSize: Long? = null,
    /** Extra overflow menu groups after the built-in entries. */
    val actions: List<List<MenuAction>> = emptyList(),
    val message: com.promtuz.chat.domain.model.MessageLocation? = null,
    val encoded: ByteArray? = null,
)

/** The one bubble video playing in place, if any. A second play, or the viewer, takes it over. */
object InlinePlayback {
    var key by mutableStateOf<String?>(null)
        private set
    private var state: VideoPlayerState? = null
    private var pending: Pair<String, VideoPlayerState>? = null

    fun play(key: String) { this.key = key }
    fun attach(key: String, s: VideoPlayerState) { if (this.key == key) state = s }
    fun stop() { key = null; state = null }

    /** The bubble lets go of its player without releasing it; the viewer picks it up by key. */
    fun handOff() {
        val k = key ?: return
        state?.let { it.handedOver = true; pending = k to it }
        stop()
    }

    fun adopt(key: String): VideoPlayerState? {
        val p = pending?.takeIf { it.first == key } ?: return null
        pending = null
        return p.second
    }

    /** Whatever was handed off and never adopted is released here. */
    fun dropPending() {
        pending?.second?.let { it.handedOver = false; it.player.release() }
        pending = null
    }
}

class MediaSession(items: List<MediaItem>, val startIndex: Int) {
    var items by mutableStateOf(items)
}

/** Updated by a renderer without recomposing the whole message for every animation frame. */
internal class MediaFrame {
    var image: ImageBitmap? = null
        set(value) {
            field = value
            if (value != null) size = IntSize(value.width, value.height)
        }
    var size by mutableStateOf(IntSize.Zero)
        private set
}

class MediaOrigin(
    val coordinates: LayoutCoordinates,
    val cornerRadius: Dp,
    val clip: (() -> Rect?)?,
    val image: (() -> ImageBitmap?)? = null,
)

/** The band a list shows its items in, in window coordinates. */
val LocalMediaClip = compositionLocalOf<(() -> Rect?)?> { null }

/** Not a route: the screen underneath stays live and keeps its scroll. */
object MediaViewer {
    var session by mutableStateOf<MediaSession?>(null)
        private set

    internal val origins = HashMap<String, MediaOrigin>()

    fun open(items: List<MediaItem>, index: Int = 0) {
        InlinePlayback.handOff()
        if (items.isEmpty()) return
        session = MediaSession(items, index.coerceIn(items.indices))
    }

    fun close() {
        session = null
        InlinePlayback.dropPending()
    }

    /** Drop an item the screen underneath just deleted; the viewer closes on the last one. */
    fun remove(key: String) {
        val s = session ?: return
        val left = s.items.filterNot { it.key == key }
        if (left.isEmpty()) session = null else s.items = left
    }

    internal fun origin(key: String): MediaOrigin? = origins[key]?.takeIf { it.coordinates.isAttached }

    internal fun originImage(key: String): ImageBitmap? = origin(key)?.image?.invoke()

    /** [key]'s visible band in [host]'s coordinates, if its list declared one. */
    fun originClipIn(key: String, host: LayoutCoordinates?): Rect? {
        val h = host?.takeIf { it.isAttached } ?: return null
        val window = origin(key)?.clip?.invoke() ?: return null
        val tl = h.windowToLocal(window.topLeft)
        val br = h.windowToLocal(window.bottomRight)
        return Rect(tl, br)
    }

    /** [key]'s rectangle in [host]'s coordinates and its corner radius in px, if it is on screen. */
    fun originRectIn(key: String, host: LayoutCoordinates?, density: Density): Pair<Rect, Float>? {
        val origin = origin(key) ?: return null
        val h = host?.takeIf { it.isAttached } ?: return null
        val topLeft = h.localPositionOf(origin.coordinates, Offset.Zero)
        val size = origin.coordinates.size
        if (size.width == 0 || size.height == 0) return null
        return Rect(topLeft, Size(size.width.toFloat(), size.height.toFloat())) to
            with(density) { origin.cornerRadius.toPx() }
    }
}

/** Marks this node as the on-screen home of [key]: the viewer opens from and closes to it. */
fun Modifier.mediaOrigin(key: String, cornerRadius: Dp, image: (() -> ImageBitmap?)? = null): Modifier =
    this then MediaOriginElement(key, cornerRadius, image)

private data class MediaOriginElement(
    val key: String, val cornerRadius: Dp, val image: (() -> ImageBitmap?)?,
) : ModifierNodeElement<MediaOriginNode>() {
    override fun create() = MediaOriginNode(key, cornerRadius, image)
    override fun update(node: MediaOriginNode) {
        if (node.key != key) node.unregister()
        node.key = key
        node.cornerRadius = cornerRadius
        node.image = image
    }
    override fun InspectorInfo.inspectableProperties() {
        name = "mediaOrigin"
        properties["key"] = key
    }
}

private class MediaOriginNode(var key: String, var cornerRadius: Dp, var image: (() -> ImageBitmap?)?) :
    Modifier.Node(), GlobalPositionAwareModifierNode, CompositionLocalConsumerModifierNode {
    private var registeredOrigin: MediaOrigin? = null

    override fun onGloballyPositioned(coordinates: LayoutCoordinates) {
        val origin = MediaOrigin(coordinates, cornerRadius, currentValueOf(LocalMediaClip)) { image?.invoke() }
        registeredOrigin = origin
        MediaViewer.origins[key] = origin
    }

    fun unregister() {
        // A recycled list node must release its frame without removing a newer origin with the same key.
        if (MediaViewer.origins[key] === registeredOrigin) MediaViewer.origins.remove(key)
        registeredOrigin = null
    }

    override fun onDetach() = unregister()
}
