package com.promtuz.chat.utils.media

import android.content.Context
import android.net.Uri
import androidx.compose.ui.graphics.ImageBitmap
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.isActive
import java.io.File

data class StagedPickedAttachment(
    val id: ULong,
    val name: String,
    val preview: ImageBitmap?,
    val image: Boolean,
)

/** Generic picker/share URIs remain untouched; processing owns an independent private copy. */
suspend fun stagePickedAttachment(context: Context, uri: Uri, conversationId: ByteArray? = null): StagedPickedAttachment {
    CoreBridge.stageImportedChatMedia(context, uri, conversationId)?.let { media ->
        return StagedPickedAttachment(
            media.id,
            if (media.image) "Photo" else "video.mp4",
            decodeEncodedPoster(media.preview, 192),
            media.image,
        )
    }
    return stagePickedDocument(context, uri, conversationId)
}

suspend fun stagePickedDocument(context: Context, uri: Uri, conversationId: ByteArray? = null): StagedPickedAttachment {
    val caller = kotlinx.coroutines.currentCoroutineContext()
    var accepted: ULong? = null
    try {
        val result = kotlinx.coroutines.withContext(kotlinx.coroutines.NonCancellable + kotlinx.coroutines.Dispatchers.IO) {
            val metadata = pickedMetadata(context, uri)
            val media = CoreBridge.importMedia(context, uri, conversationId)
            val id = try { media.stageAttachment(metadata.first, metadata.second).also { accepted = it } }
                catch (error: Throwable) { runCatching { media.discard() }; throw error }
            StagedPickedAttachment(id, metadata.first, null, false)
        }
        caller.ensureActive()
        return result
    } finally {
        if (!caller.isActive) accepted?.let { kotlinx.coroutines.withContext(kotlinx.coroutines.NonCancellable) { CoreBridge.discardStaged(it) } }
    }
}

private fun pickedMetadata(context: Context, uri: Uri): Pair<String, String> {
    val source = uri.path?.takeIf { uri.scheme == "file" }?.let(::File)
    val name = source?.name ?: context.contentResolver.query(uri,
        arrayOf(android.provider.OpenableColumns.DISPLAY_NAME), null, null, null)?.use {
        if (it.moveToFirst() && !it.isNull(0)) it.getString(0) else null
    } ?: "file"
    val extension = name.substringAfterLast('.', "").lowercase(java.util.Locale.ROOT)
    val mime = context.contentResolver.getType(uri)?.takeUnless { it == "application/octet-stream" }
        ?: android.webkit.MimeTypeMap.getSingleton().getMimeTypeFromExtension(extension) ?: "application/octet-stream"
    return name to mime
}

/** Called only with a finalized clip created by CameraOverlay, transferring its ownership. */
suspend fun stageCapturedVideo(context: Context, file: File): StagedPickedAttachment =
    try { stagePickedAttachment(context, Uri.fromFile(file)) } finally { file.delete() }
