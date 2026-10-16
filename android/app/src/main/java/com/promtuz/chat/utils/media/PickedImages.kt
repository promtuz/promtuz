package com.promtuz.chat.utils.media

import android.content.Context
import android.net.Uri
import androidx.compose.ui.graphics.ImageBitmap
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

class ImagePreparationException(message: String) : Exception(message)

data class StagedPickedImage(val id: ULong, val preview: ImageBitmap?)

suspend fun stagePickedImage(context: Context, uri: Uri): StagedPickedImage = withContext(Dispatchers.IO) {
    val staged = CoreBridge.stageImportedChatImage(context, uri)
    StagedPickedImage(staged.id, decodeEncodedPoster(staged.avif, 192))
}
