package com.promtuz.chat.presentation.viewmodel

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.*
import org.junit.Assert.assertEquals
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class OutgoingTypingTest {
    @Test fun peerReturningReceivesCurrentTypingEvenIfFirstSignalWasDropped() = runTest {
        val received = mutableListOf<Boolean>()
        var reachable = false
        val typing = OutgoingTyping(backgroundScope, { testScheduler.currentTime }) {
            if (reachable) received += it
        }
        typing.setForeground(true)
        typing.edited("Hello")
        runCurrent()
        assertEquals(emptyList<Boolean>(), received)
        advanceTimeBy(500)
        reachable = true
        typing.refresh()
        runCurrent()
        assertEquals(listOf(true), received)
        advanceTimeBy(5500)
        runCurrent()
        assertEquals(listOf(true, false), received)
        typing.refresh()
        runCurrent()
        assertEquals(listOf(true, false), received)
    }

    @Test fun leavingChatOrClearingDraftStopsTypingAndReconnectCannotReviveIt() = runTest {
        val signals = mutableListOf<Boolean>()
        val typing = OutgoingTyping(backgroundScope, { testScheduler.currentTime }) { signals += it }
        typing.setForeground(true)
        typing.edited("Draft")
        runCurrent()
        typing.setForeground(false)
        runCurrent()
        typing.refresh()
        typing.setForeground(true)
        typing.refresh()
        runCurrent()
        assertEquals(listOf(true, false), signals)
        typing.edited("Draft edited")
        runCurrent()
        typing.edited("")
        runCurrent()
        typing.refresh()
        runCurrent()
        assertEquals(listOf(true, false, true, false), signals)
    }
}
