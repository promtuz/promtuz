package com.promtuz.chat.ui.screens

import com.promtuz.chat.domain.model.*
import org.junit.Assert.*
import org.junit.Test

class TypingGroupingTest {
    private val message = UiMessage("1", "1", null, MessageContent.Text("Hello"), false,
        senderHex = "alice", status = SendStatus.Delivered, edited = false, deleted = false,
        timestampMs = 1000, reactions = emptyList())

    @Test fun onlyAnUnambiguousSameAuthorContinuesTheGroup() {
        assertTrue(typingJoinsMessage(message, setOf("alice"), true, 1500, 1000))
        assertFalse(typingJoinsMessage(message, setOf("bob"), true, 1500, 1000))
        assertFalse(typingJoinsMessage(message, setOf("alice", "bob"), true, 1500, 1000))
        assertFalse(typingJoinsMessage(message.copy(outgoing = true), setOf("alice"), true, 1500, 1000))
    }

    @Test fun directChatStillRespectsTimeAndSystemMessageBoundaries() {
        val direct = message.copy(senderHex = null)
        assertTrue(typingJoinsMessage(direct, setOf("alice"), false, 1500, 1000))
        assertFalse(typingJoinsMessage(direct, setOf("alice"), false, 2001, 1000))
        assertFalse(typingJoinsMessage(null, setOf("alice"), false, 1500, 1000))
        assertFalse(typingJoinsMessage(direct.copy(content = MessageContent.System(SystemEventKind.entries.first(), "Alice", "")),
            setOf("alice"), false, 1500, 1000))
    }
}
