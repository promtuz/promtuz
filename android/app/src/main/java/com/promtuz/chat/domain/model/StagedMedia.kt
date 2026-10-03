package com.promtuz.chat.domain.model

import androidx.compose.ui.graphics.ImageBitmap

const val STAGED_READY = 1
const val STAGED_FAILED = 2

const val STAGED_IMAGE = 1
const val STAGED_ATTACHMENT = 2

/**
 * Mirrors libcore's staging registry. [preview] is decoded by the client for an image, so the
 * strip draws before the AVIF pass lands, and is libcore's blurred thumb for an attachment.
 */
data class StagedMedia(
    val id: ULong,
    val kind: Int,
    val state: Int,
    val name: String,
    val mime: String,
    val size: Long,
    val width: Int,
    val height: Int,
    val preview: ImageBitmap?,
    val error: String?,
) {
    val ready: Boolean get() = state == STAGED_READY
    val failed: Boolean get() = state == STAGED_FAILED
}

/** Must match libcore's revision rules, so the composer never offers a swap the core refuses. */
fun MessageContent.acceptsStaged(kind: Int): Boolean = when (this) {
    is MessageContent.Text -> kind == STAGED_IMAGE
    is MessageContent.Image -> kind == STAGED_IMAGE
    is MessageContent.Attachment -> kind == STAGED_ATTACHMENT
    // A revision targets one message and an album is several.
    is MessageContent.Album -> false
    is MessageContent.System, is MessageContent.Call -> false
    is MessageContent.Voice, is MessageContent.Sticker -> false
}
