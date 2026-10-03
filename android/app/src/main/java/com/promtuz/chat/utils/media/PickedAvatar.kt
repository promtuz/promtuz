package com.promtuz.chat.utils.media

import android.content.Context
import android.graphics.Bitmap
import android.net.Uri
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.graphics.asImageBitmap
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

sealed interface AvatarSource {
    val poster: ImageBitmap
    val canCrop: Boolean
    val width: Int get() = poster.width
    val height: Int get() = poster.height

    class Still(val bitmap: Bitmap) : AvatarSource {
        override val poster = bitmap.asImageBitmap()
        override val canCrop = true
    }

    class Encoded(
        val original: ByteArray,
        val picture: AvatarPicture,
        override val canCrop: Boolean,
        override val width: Int = picture.poster.width,
        override val height: Int = picture.poster.height,
    ) : AvatarSource {
        override val poster get() = picture.poster
    }
}

/** Use the same source/color inspection as photos and stickers before opening a crop editor. */
suspend fun pickAvatarSource(context: Context, uri: Uri): AvatarSource {
    val source = pickImageSource(context, uri, maxEdge = 1024, keepBitmap = true)
    if (source.preserveOriginal) {
        throw ImagePreparationException("Use an AVIF to keep this image’s HDR or colors in a profile photo.")
    }
    source.bitmap?.let { return AvatarSource.Still(it) }
    val original = source.encoded ?: throw ImagePreparationException("Couldn’t open this photo.")
    val gif = original.size >= 6 && String(original, 0, 6, Charsets.US_ASCII) in setOf("GIF87a", "GIF89a")
    if (!gif) {
        val info = withContext(Dispatchers.Default) { CoreBridge.inspectAvif(original) }
            ?: throw ImagePreparationException("Couldn’t open this photo.")
        val ordinaryStill = !info.animated && info.bitDepth.toInt() == 8 &&
            !info.hasGainMap && !info.hasIcc && info.colorPrimaries?.toInt() in setOf(1, 2) &&
            info.transferCharacteristics?.toInt() in setOf(1, 2, 13)
        if (ordinaryStill) {
            val bitmap = withContext(Dispatchers.Default) {
                decodeEncodedPoster(original, 1024)?.asAndroidBitmap()
            } ?: throw ImagePreparationException("Couldn’t open this photo.")
            return AvatarSource.Still(bitmap)
        }
    }
    // GIF gets an animated preview and a source-relative crop on Save. AVIF keeps its exact
    // container, so the editor shows the existing centered avatar framing without pixel edits.
    val prepared = CoreBridge.prepareAvatarImage(original)
    val picture = prepareAvatar(prepared.bytes)
        ?: throw ImagePreparationException("This phone couldn’t display that profile photo.")
    // Core has validated the GIF canvas. Keep its original dimensions for crop coordinates;
    // the fitted preview's rounded aspect ratio must not move the saved crop's edges.
    fun gifDimension(at: Int) = (original[at].toInt() and 255) or ((original[at + 1].toInt() and 255) shl 8)
    return AvatarSource.Encoded(original, picture, canCrop = gif,
        width = if (gif) gifDimension(6) else picture.poster.width,
        height = if (gif) gifDimension(8) else picture.poster.height,
    )
}
