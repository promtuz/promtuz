package com.promtuz.chat.domain.model

import androidx.compose.runtime.Immutable
import androidx.compose.ui.graphics.ImageBitmap

/**
 * A message's payload; the bubble switches on the variant. Media variants hold
 * pre-decoded, process-cached [ImageBitmap]s (never raw ByteArray) so their
 * value-equality is stable across reactive re-reads.
 */
@Immutable
sealed interface MessageContent {
    data class Text(val text: String) : MessageContent

    /** Inline image; [bitmap] is null when this API level can't decode AVIF. */
    data class Image(
        val caption: String,
        val bitmap: ImageBitmap?,
        val width: Int,
        val height: Int,
    ) : MessageContent

    /**
     * Several media messages sharing a `group_id`, drawn as one unit.
     *
     * Each item stays its own message on the wire and in storage — its own
     * dispatch id, status and transfer — which is what lets a later pick join an
     * existing album instead of landing at the bottom as a separate bubble. Only
     * the rendering is collapsed. The caption rides the first item sent, so it's
     * lifted here rather than left buried in one member.
     */
    data class Album(
        val caption: String,
        val items: List<AlbumItem>,
    ) : MessageContent

    /** P2P attachment pulled by [fileIdHex]; [transferState] 0 none/1 active/2 done/3 failed/4 held. */
    data class Attachment(
        val caption: String,
        val name: String,
        val size: Long,
        val mime: String,
        val thumb: ImageBitmap?,
        val fileIdHex: String,
        val transferState: Int,
        val transferHave: Int,
        val transferTotal: Int,
        val localPath: String?,
    ) : MessageContent
}

/** One member of an [MessageContent.Album], still addressable by its own id. */
@Immutable
data class AlbumItem(val dispatchIdHex: String, val content: MessageContent)
