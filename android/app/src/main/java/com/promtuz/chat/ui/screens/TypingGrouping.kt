package com.promtuz.chat.ui.screens

import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.UiMessage
import kotlin.math.abs

/** A combined typing indicator can continue a run only when its author is unambiguous. */
internal fun typingJoinsMessage(
    newest: UiMessage?, typingMembers: Set<String>, isGroup: Boolean, nowMs: Long, windowMs: Long,
): Boolean = newest != null && !newest.outgoing && newest.content !is MessageContent.System &&
    typingMembers.size == 1 && (!isGroup || newest.senderHex == typingMembers.single()) &&
    abs(nowMs - newest.timestampMs) <= windowMs
