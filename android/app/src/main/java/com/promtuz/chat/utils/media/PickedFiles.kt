package com.promtuz.chat.utils.media

import android.content.Context
import android.net.Uri
import androidx.compose.ui.graphics.ImageBitmap
import com.promtuz.core.CoreBridge
import java.io.File

data class StagedPickedAttachment(
    val id: ULong,
    val name: String,
    val preview: ImageBitmap?,
    val image: Boolean,
)

/** Generic picker/share URIs remain untouched; processing owns an independent private copy. */
suspend fun stagePickedAttachment(context: Context, uri: Uri): StagedPickedAttachment {
    CoreBridge.stageImportedChatMedia(context, uri)?.let { media ->
        return StagedPickedAttachment(
            media.id,
            if (media.image) "Photo" else "video.mp4",
            decodeEncodedPoster(media.preview, 192),
            media.image,
        )
    }
    return stagePickedDocument(context, uri)
}

suspend fun stagePickedDocument(context: Context, uri: Uri): StagedPickedAttachment {
    val picked = resolvePickedFile(context, uri)
        ?: throw MediaPreparationException("Couldn’t open this attachment.")
    val original = File(picked.path)
    val id = stageOwnedFile(original,
        stage = { CoreBridge.stageRawAttachment(original.absolutePath, picked.name, picked.mime) },
        discard = { CoreBridge.discardStaged(it) },
    )
    return StagedPickedAttachment(id, picked.name, null, false)
}

/** Called only with a finalized clip created by CameraOverlay, transferring its ownership. */
suspend fun stageCapturedVideo(context: Context, file: File): StagedPickedAttachment =
    try { stagePickedAttachment(context, Uri.fromFile(file)) } finally { file.delete() }
