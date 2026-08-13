package com.promtuz.chat.domain.model

import androidx.compose.ui.graphics.ImageBitmap

/** Still encoding — the strip draws a progress ring over the preview. */
const val STAGED_PREPARING = 0
const val STAGED_READY = 1
const val STAGED_FAILED = 2

const val STAGED_IMAGE = 1
const val STAGED_ATTACHMENT = 2

/**
 * One item in the composer buffer. Mirrors libcore's staging registry, plus a
 * pre-decoded [preview] — the core hands back no encoded bytes for an image
 * (the client already holds the pick), and the strip needs something to draw
 * from the moment it appears, well before the AVIF pass lands.
 *
 * For an attachment the preview is libcore's blurred thumb; a file with no
 * visual preview has none and the tile falls back to a glyph.
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

/**
 * Whether a staged pick of [kind] may replace this message's body — the client
 * half of libcore's revision matrix, so the composer never offers a swap the
 * core will refuse. Text and image are both frame-carried and interchange; an
 * attachment is fetched device-to-device and stays on its own diagonal.
 */
fun MessageContent.acceptsStaged(kind: Int): Boolean = when (this) {
    is MessageContent.Text -> kind == STAGED_IMAGE
    is MessageContent.Image -> kind == STAGED_IMAGE
    is MessageContent.Attachment -> kind == STAGED_ATTACHMENT
    // An album is several messages wearing one bubble; a revision targets exactly
    // one, so there's no single body for a pick to replace.
    is MessageContent.Album -> false
    // Nobody authored a system row, so there is nothing to revise.
    is MessageContent.System -> false
}
