package com.promtuz.chat.domain.model

import androidx.compose.runtime.Immutable
import androidx.compose.ui.graphics.ImageBitmap

/** Media variants keep a cached poster alongside their original encoded image. */
@Immutable
sealed interface MessageContent {
    data class Text(val text: String) : MessageContent

    /** Inline image; [bitmap] is a poster, while [encoded] retains animation and color information. */
    data class Image(
        val caption: String,
        val bitmap: ImageBitmap?,
        val width: Int,
        val height: Int,
        val encoded: ByteArray? = null,
        val mime: String = "image/avif",
    ) : MessageContent {
        override fun equals(other: Any?) = other is Image &&
            caption == other.caption && bitmap == other.bitmap &&
            width == other.width && height == other.height && mime == other.mime &&
            (encoded === other.encoded ||
                (encoded != null && other.encoded != null && encoded.contentEquals(other.encoded)))

        override fun hashCode(): Int {
            var result = caption.hashCode()
            result = 31 * result + (bitmap?.hashCode() ?: 0)
            result = 31 * result + width
            result = 31 * result + height
            result = 31 * result + (encoded?.contentHashCode() ?: 0)
            return 31 * result + mime.hashCode()
        }
    }

    /** Media messages sharing a `group_id`, drawn as one unit; each item stays its own message. */
    data class Album(
        val caption: String,
        val items: List<AlbumItem>,
    ) : MessageContent

    data class System(val event: SystemEventKind, val actor: String, val target: String, val detail: String = "") :
        MessageContent

    /** P2P attachment pulled by [fileIdHex]; [transferState] 0 none/1 active/2 done/3 failed/4 held/5 connecting. */
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

    data class Sticker(val sticker: StickerRef) : MessageContent

    /**
     * [durationSecs] is set only when the call connected; otherwise [missed] tells
     * a missed call from a declined or unanswered one.
     */
    data class Call(
        val outgoing: Boolean,
        val durationSecs: Int?,
        val missed: Boolean,
    ) : MessageContent

    /** [waveform] holds the sender's loudness samples (0..255); [bytes] is the encoded audio. */
    data class Voice(
        val dispatchIdHex: String,
        val mime: String,
        val durationMs: Int,
        val waveform: ByteArray,
        val bytes: ByteArray,
    ) : MessageContent {
        // Equal by dispatch id: the arrays never change for an id, and a byte-wise
        // compare of every recording on recomposition is too costly.
        override fun equals(other: Any?) = other is Voice && other.dispatchIdHex == dispatchIdHex
        override fun hashCode() = dispatchIdHex.hashCode()
    }
}

fun mediaLabel(kind: Int, name: String = ""): String = when (kind) {
    1 -> "Photo"
    2 -> name.ifEmpty { "File" }
    3 -> "Voice message"
    4 -> "Sticker"
    else -> ""
}

/** One line for a reply bar or an info header. */
fun MessageContent.previewLine(): String = when (this) {
    is MessageContent.Text -> text
    is MessageContent.Image -> caption.ifBlank { mediaLabel(1) }
    is MessageContent.Album -> caption.ifBlank { "${items.size} photos" }
    is MessageContent.Attachment -> caption.ifBlank { mediaLabel(2, name) }
    is MessageContent.Voice -> mediaLabel(3)
    is MessageContent.Sticker -> mediaLabel(4)
    is MessageContent.System, is MessageContent.Call -> ""
}

enum class SystemEventKind { Added, Left, Removed, Titled, Role, Rules }

/**
 * [target] is a member hex for membership events, the new name for a rename,
 * `<member hex>:<role>` for a role change and `<rule>:<0|1>` for a rule change.
 */
fun systemContent(code: Int, actorHex: String?, target: String, names: Map<String, String>): MessageContent.System {
    val (subject, detail) = if (code == 6 || code == 7) target.substringBefore(':') to target.substringAfter(':', "")
        else target to ""
    return MessageContent.System(
        event = when (code) {
            1 -> SystemEventKind.Added
            2 -> SystemEventKind.Left
            3 -> SystemEventKind.Removed
            6 -> SystemEventKind.Role
            7 -> SystemEventKind.Rules
            else -> SystemEventKind.Titled
        },
        actor = actorHex?.let { names[it] } ?: "Someone",
        target = if (code == 4 || code == 7) subject else names[subject] ?: "someone",
        detail = detail,
    )
}

@Immutable
data class AlbumItem(val dispatchIdHex: String, val content: MessageContent)
