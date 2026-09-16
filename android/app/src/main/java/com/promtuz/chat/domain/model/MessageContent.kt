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

    /**
     * A membership or title change, narrated between the messages it happened
     * between. Not a bubble — a centred line with no author, no reactions and
     * nothing to reply to.
     */
    data class System(val event: SystemEventKind, val actor: String, val target: String) :
        MessageContent

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

    /** The reference supplies dimensions before the image is downloaded. */
    data class Sticker(val sticker: StickerRef) : MessageContent

    /**
     * Inline voice note. [waveform] is the sender's loudness samples (0–255),
     * enough to draw the bubble before anything decodes; [bytes] is the encoded
     * audio, handed to the player on the first tap.
     */
    data class Voice(
        val dispatchIdHex: String,
        val mime: String,
        val durationMs: Int,
        val waveform: ByteArray,
        val bytes: ByteArray,
    ) : MessageContent {
        // Identity equality: the arrays are immutable and keyed by dispatch id,
        // and a byte-wise compare of every recording on every recomposition is
        // the wrong trade.
        override fun equals(other: Any?) = other is Voice && other.dispatchIdHex == dispatchIdHex
        override fun hashCode() = dispatchIdHex.hashCode()
    }
}

/** One line standing in for a media message wherever its body can't be shown. */
fun mediaLabel(kind: Int, name: String = ""): String = when (kind) {
    1 -> "Photo"
    2 -> name.ifEmpty { "File" }
    3 -> "Voice message"
    4 -> "Sticker"
    else -> ""
}

/** What a [MessageContent.System] row is narrating. */
enum class SystemEventKind { Added, Left, Removed, Titled }

/** One member of an [MessageContent.Album], still addressable by its own id. */
@Immutable
data class AlbumItem(val dispatchIdHex: String, val content: MessageContent)
