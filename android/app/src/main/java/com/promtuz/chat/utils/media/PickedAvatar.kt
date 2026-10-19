package com.promtuz.chat.utils.media

import android.content.Context
import android.graphics.Bitmap
import android.net.Uri
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap

data class AvatarSource(val uri: Uri, val bitmap: Bitmap) {
    val poster: ImageBitmap = bitmap.asImageBitmap()
    val width get() = bitmap.width
    val height get() = bitmap.height
}

suspend fun pickAvatarSource(context: Context, uri: Uri): AvatarSource {
    val bitmap = decodeDownscaled(context, uri, 1024)
        ?: throw MediaPreparationException("Couldn’t open this photo.")
    return AvatarSource(uri, bitmap)
}
