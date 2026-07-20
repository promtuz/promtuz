package com.promtuz.chat.ui.components

import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.UiMessage

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
        }

    fun metaLabelOf(msg: UiMessage): String = buildString {
        if (msg.edited && !msg.deleted) append("edited ")
        append(clock(msg.timestampMs))
    }

    private val clockFormat = java.text.SimpleDateFormat("HH:mm", java.util.Locale.getDefault())
    fun clock(ms: Long): String = clockFormat.format(java.util.Date(ms))
}
