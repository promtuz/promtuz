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
