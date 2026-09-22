package com.promtuz.chat.ui.components

import androidx.compose.ui.unit.dp
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.SystemEventKind
import com.promtuz.chat.domain.model.UiMessage

/** Stable across every outgoing status, including pending and failed. */
internal val BubbleStatusSize = 16.dp
internal val BubbleStatusGap = 3.dp

/**
 * Static helpers for a message's display text and its meta label (send time,
 * "edited" prefix, "deleted" placeholder). Draw-time only; no layout cache.
 */
object BubbleTextLayouts {
    fun contentOf(msg: UiMessage): String =
        if (msg.deleted) "This message was deleted"
        else when (val c = msg.content) {
            is MessageContent.Text -> c.text
            is MessageContent.Image -> c.caption
            is MessageContent.Attachment -> c.caption
            is MessageContent.Album -> c.caption
            is MessageContent.System -> systemLine(c)
            // A call row draws its own line with an icon, never as bubble text.
            is MessageContent.Call, is MessageContent.Voice, is MessageContent.Sticker -> ""
        }

    /** The narration for a membership or title change, in the past tense. */
    fun systemLine(c: MessageContent.System): String = when (c.event) {
        SystemEventKind.Added -> "${c.actor} added ${c.target}"
        SystemEventKind.Removed -> "${c.actor} removed ${c.target}"
        SystemEventKind.Left -> "${c.target} left"
        SystemEventKind.Titled -> "${c.actor} named the group \"${c.target}\""
    }

    fun metaLabelOf(msg: UiMessage): String = buildString {
        if (msg.edited && !msg.deleted) append("edited ")
        append(clock(msg.timestampMs))
    }

    private val clockFormat = java.text.SimpleDateFormat("HH:mm", java.util.Locale.getDefault())
    fun clock(ms: Long): String = clockFormat.format(java.util.Date(ms))
}
