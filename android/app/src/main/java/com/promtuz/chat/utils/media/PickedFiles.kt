package com.promtuz.chat.utils.media

import android.content.Context
import android.net.Uri
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.withContext
import timber.log.Timber
import uniffi.core.VideoPreparation
import java.io.File

data class StagedPickedAttachment(val id: ULong, val name: String, val preview: ImageBitmap?)

/** Generic picker/share URIs remain untouched; processing owns an independent private copy. */
suspend fun stagePickedAttachment(context: Context, uri: Uri): StagedPickedAttachment {
    val copy = resolvePickedFile(context, uri)
        ?: throw ImagePreparationException("Couldn’t open this attachment.")
    return stagePrivateAttachment(context, copy)
}

/** Called only with a finalized clip created by CameraOverlay, transferring its ownership. */
suspend fun stageCapturedVideo(context: Context, file: File): StagedPickedAttachment =
    stagePrivateAttachment(context, PickedFile(file.absolutePath, file.name, "video/mp4"))

private suspend fun stagePrivateAttachment(context: Context, picked: PickedFile): StagedPickedAttachment {
    if (picked.mime.startsWith("video/")) return stagePrivateVideo(context, picked)
    val original = File(picked.path)
    val pending = linkedSetOf(original)
    try {
        currentCoroutineContext().ensureActive()
        val uri = Uri.fromFile(original)
        val bitmap = if (picked.mime.startsWith("image/")) decodeDownscaled(context, uri, 320) else null
        val preview = bitmap?.asImageBitmap()
        val rgba = withContext(Dispatchers.Default) { bitmap?.toRgba() }

        pending.remove(original) // stageOwnedFile owns it from this call onward, including failures.
        val id = stageOwnedFile(original,
            stage = { CoreBridge.stageAttachment(original.absolutePath, picked.name, picked.mime,
                rgba, bitmap?.width ?: 0, bitmap?.height ?: 0) },
            discard = { CoreBridge.discardStaged(it) },
        )
        return StagedPickedAttachment(id, picked.name, preview)
    } finally {
        if (pending.isNotEmpty()) withContext(NonCancellable + Dispatchers.IO) {
            pending.forEach { it.delete() }
        }
    }
}

/** The host executes core's plan; core owns candidates, acceptance and the staging handoff. */
private suspend fun stagePrivateVideo(context: Context, picked: PickedFile): StagedPickedAttachment {
    val caller = currentCoroutineContext()
    val original = File(picked.path)
    var preparation: VideoPreparation? = null
    var accepted: ULong? = null
    try {
        val staged: StagedPickedAttachment
        try {
            caller.ensureActive()
            val input = inspectVideo(context, original)
            // Capture the object before a cancelled IO return can lose its ownership transfer.
            withContext(NonCancellable + Dispatchers.IO) {
                preparation = VideoPreparation(picked.path, picked.name, picked.mime, input)
            }
            caller.ensureActive()
            val job = checkNotNull(preparation)
            val plan = job.plan()
            val output = if (plan != null) {
                val file = File(job.outputPath())
                encodeVideo(context, File(job.sourcePath()), file, input, plan)
                inspectVideo(context, file)
            } else null
            val selected = withContext(Dispatchers.IO) { job.select(output) }
            val bitmap = videoPoster(context, Uri.fromFile(File(selected.path)), selected.posterEdge.toInt())?.first
            val preview = bitmap?.asImageBitmap()
            val rgba = withContext(Dispatchers.Default) { bitmap?.toRgba() }
            val id = withContext(NonCancellable + Dispatchers.IO) {
                job.stage(rgba, (bitmap?.width ?: 0).toUInt(), (bitmap?.height ?: 0).toUInt())
                    .also { accepted = it }
            }
            staged = StagedPickedAttachment(id, selected.name, preview)
        } finally {
            // encodeVideo releases its codecs before it returns or throws. Only now can core
            // release private candidates; an accepted staging ID keeps its own file reference.
            withContext(NonCancellable + Dispatchers.IO) {
                val job = preparation
                if (job == null) original.delete() // Constructor never accepted ownership.
                else try { job.cancel() } finally { job.destroy() }
            }
        }
        // No suspension remains between delivery and the caller recording this ID.
        caller.ensureActive()
        return staged
    } catch (error: Throwable) {
        accepted?.let { id -> withContext(NonCancellable) {
            runCatching { CoreBridge.discardStaged(id) }
                .onFailure { Timber.tag("VideoPreparation").w(it, "Could not discard interrupted video preparation") }
        } }
        throw error
    }
}
