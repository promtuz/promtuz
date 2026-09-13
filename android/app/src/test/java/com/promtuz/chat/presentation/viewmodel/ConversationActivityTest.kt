package com.promtuz.chat.presentation.viewmodel

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class ConversationActivityTest {
    @Test fun openingChatReadsExistingActivityWithoutExtendingExpiry() = runTest {
        val activity = ConversationActivity(this, 6000)
        activity.update("chat", "alice", 1)
        runCurrent()
        advanceTimeBy(5000)
        assertEquals(1, activity.byChat.value["chat"])
        // A newly opened chat reads exactly the snapshot used by the home list.
        assertEquals(mapOf("alice" to 1), activity.members.value["chat"])
        advanceTimeBy(1000)
        runCurrent()
        assertTrue(activity.members.value.isEmpty())
        assertTrue(activity.byChat.value.isEmpty())
    }

    @Test fun stoppingOneMemberDoesNotClearOthersOrAnotherConversation() = runTest {
        val activity = ConversationActivity(this, 1000)
        activity.update("group", "alice", 1)
        activity.update("group", "bob", 2)
        activity.update("direct", "alice", 1)
        assertEquals(3, activity.byChat.value["group"])
        activity.update("group", "alice", 0)
        assertEquals(mapOf("bob" to 2), activity.members.value["group"])
        assertEquals(1, activity.byChat.value["direct"])
    }

    @Test fun refreshAndExpiryBelongToEachMember() = runTest {
        val activity = ConversationActivity(this, 1000)
        activity.update("group", "alice", 1)
        activity.update("group", "bob", 1)
        runCurrent()
        advanceTimeBy(500)
        activity.update("group", "bob", 1)
        runCurrent()
        advanceTimeBy(500)
        runCurrent()
        assertEquals(mapOf("bob" to 1), activity.members.value["group"])
        advanceTimeBy(500)
        runCurrent()
        assertTrue(activity.members.value.isEmpty())
    }
}
