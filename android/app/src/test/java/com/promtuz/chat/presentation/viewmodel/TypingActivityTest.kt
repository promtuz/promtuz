package com.promtuz.chat.presentation.viewmodel

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class TypingActivityTest {
    @Test fun membersExpireIndependentlyAndRefreshOnlyTheirOwnTimer() = runTest {
        val activity = TypingActivity(this, 1_000)
        activity.update("alice", true)
        runCurrent()
        advanceTimeBy(400)
        activity.update("bob", true)
        runCurrent()
        advanceTimeBy(400)
        activity.update("bob", true)
        runCurrent()
        advanceTimeBy(200)
        runCurrent()
        assertEquals(setOf("bob"), activity.members.value)
        assertTrue(activity.typing.value)
        advanceTimeBy(800)
        runCurrent()
        assertEquals(emptySet<String>(), activity.members.value)
        assertFalse(activity.typing.value)
    }

    @Test fun idleOrIncomingMessageClearsOnlyItsAuthor() = runTest {
        val activity = TypingActivity(this, 1_000)
        activity.update("alice", true)
        activity.update("bob", true)
        runCurrent()
        activity.update("alice", false)
        assertEquals(setOf("bob"), activity.members.value)
        assertTrue(activity.typing.value)
        advanceTimeBy(1_000)
        runCurrent()
        assertFalse(activity.typing.value)
    }

    @Test fun clearingCannotLeaveOldMembersOrTimersBehind() = runTest {
        val activity = TypingActivity(this, 1_000)
        activity.update("alice", true)
        activity.update("bob", true)
        runCurrent()
        advanceTimeBy(500)
        activity.clear()
        assertEquals(emptySet<String>(), activity.members.value)
        assertFalse(activity.typing.value)
        activity.update("alice", true)
        runCurrent()
        advanceTimeBy(500)
        runCurrent()
        assertEquals(setOf("alice"), activity.members.value)
        advanceTimeBy(500)
        runCurrent()
        assertFalse(activity.typing.value)
    }
}
