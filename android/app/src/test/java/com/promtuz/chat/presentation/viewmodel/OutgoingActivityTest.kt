package com.promtuz.chat.presentation.viewmodel

import com.promtuz.chat.domain.model.Activity
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.*
import org.junit.Assert.assertEquals
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class OutgoingActivityTest {
    @Test fun peerReturningReceivesCurrentTypingEvenIfFirstSignalWasDropped() = runTest {
        val received = mutableListOf<Boolean>()
        var reachable = false
        val typing = OutgoingActivity(backgroundScope, { testScheduler.currentTime }) {
            if (reachable) received += (it != 0)
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
        val typing = OutgoingActivity(backgroundScope, { testScheduler.currentTime }) { signals += (it != 0) }
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
    @Test fun stickerSelectionSurvivesResumeAndTypingExpiryCannotClearIt() = runTest {
        val sent = mutableListOf<Int>()
        val activity = OutgoingActivity(backgroundScope, { testScheduler.currentTime }) { sent += it }
        val choosing = Activity.ChoosingSticker.bit
        activity.setForeground(true)
        activity.edited("Draft")
        runCurrent()
        activity.setChoosingSticker(true)
        runCurrent()
        advanceTimeBy(6001)
        runCurrent()
        assertEquals(choosing, sent.last())
        activity.setForeground(false)
        runCurrent()
        assertEquals(0, sent.last())
        activity.setForeground(true)
        runCurrent()
        assertEquals(choosing, sent.last())
        activity.setChoosingSticker(false)
        runCurrent()
        assertEquals(0, sent.last())
        activity.edited("Continue typing")
        runCurrent()
        assertEquals(Activity.Typing.bit, sent.last())
    }
}
