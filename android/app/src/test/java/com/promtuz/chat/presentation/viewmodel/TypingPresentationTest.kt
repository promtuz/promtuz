package com.promtuz.chat.presentation.viewmodel

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.*
import org.junit.Assert.*
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class TypingPresentationTest {
    @Test fun stopBeforeMessageRetainsTheSurfaceButAnAbandonedDraftExpires() = runTest {
        val activity = ConversationActivity(this, 6000)
        val presentation = TypingPresentation(this)
        activity.update("chat", "alice", 1)
        presentation.update(activity.members.value["chat"].orEmpty().keys)
        activity.update("chat", "alice", 0)
        presentation.update(activity.members.value["chat"].orEmpty().keys)
        runCurrent()
        advanceTimeBy(400)
        assertTrue(activity.members.value.isEmpty())
        assertEquals(setOf("alice"), presentation.members.value)
        presentation.consume("alice") // Message arrives after the stop signal.
        assertTrue(presentation.members.value.isEmpty())
        presentation.update(setOf("alice"))
        presentation.update(emptySet())
        runCurrent()
        advanceTimeBy(600)
        runCurrent()
        assertTrue(presentation.members.value.isEmpty())
    }

    @Test fun resumedTypingAndOtherMembersSurviveAnOldStop() = runTest {
        val presentation = TypingPresentation(this)
        presentation.update(setOf("alice", "bob"))
        presentation.update(setOf("bob"))
        runCurrent()
        advanceTimeBy(400)
        presentation.update(setOf("alice", "bob"))
        advanceTimeBy(300)
        runCurrent()
        assertEquals(setOf("alice", "bob"), presentation.members.value)
        presentation.consume("alice")
        assertEquals(setOf("bob"), presentation.members.value)
    }
}
