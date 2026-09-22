package com.promtuz.chat.ui.screens

import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.UiMessage
import java.time.Instant
import java.time.LocalDate
import java.time.ZoneId
import kotlin.math.abs

internal sealed interface ChatRow {
    data class Msg(val msg: UiMessage, val mergedTop: Boolean, val mergedBottom: Boolean) : ChatRow
    data class System(val msg: UiMessage) : ChatRow
    data class Typing(val mergedTop: Boolean) : ChatRow
    data class Date(val date: LocalDate, val oldestMessageKey: String) : ChatRow
}

/** Newest-first rows; a date follows its messages so it appears above them on the stage. */
internal fun buildChatRows(
    messages: List<UiMessage>,
    mergeWindowMs: Long,
    typing: Boolean,
    typingMembers: Set<String>,
    isGroup: Boolean,
    nowMs: Long,
    zone: ZoneId,
): List<ChatRow> {
    val dates = messages.map { Instant.ofEpochMilli(it.timestampMs).atZone(zone).toLocalDate() }
    val today = Instant.ofEpochMilli(nowMs).atZone(zone).toLocalDate()
    val joinsTyping = typing && dates.firstOrNull() == today &&
        typingJoinsMessage(messages.firstOrNull(), typingMembers, isGroup, nowMs, mergeWindowMs)
    val rows = ArrayList<ChatRow>(messages.size + 1)
    if (typing) rows.add(ChatRow.Typing(joinsTyping))
    for (i in messages.indices) {
        val message = messages[i]
        val date = dates[i]
        val older = messages.getOrNull(i + 1)
        val newer = messages.getOrNull(i - 1)
        val mergedTop = older != null && date == dates[i + 1] && sameGroup(message, older, mergeWindowMs)
        val mergedBottom = (i == 0 && joinsTyping) ||
            (newer != null && date == dates[i - 1] && sameGroup(message, newer, mergeWindowMs))
        rows.add(
            if (message.content is MessageContent.System || message.content is MessageContent.Call)
                ChatRow.System(message)
            else ChatRow.Msg(message, mergedTop, mergedBottom)
        )
        if (date != dates.getOrNull(i + 1)) {
            // Anchor the divider to its run, even when a late message repeats a
            // date. Pagination only replaces the divider at the history edge.
            rows.add(ChatRow.Date(date, message.key))
        }
    }
    return rows
}

private fun sameGroup(a: UiMessage, b: UiMessage, windowMs: Long): Boolean =
    a.outgoing == b.outgoing &&
        a.senderHex == b.senderHex &&
        a.content !is MessageContent.System && b.content !is MessageContent.System &&
        a.content !is MessageContent.Call && b.content !is MessageContent.Call &&
        abs(a.timestampMs - b.timestampMs) <= windowMs
