package com.promtuz.chat.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toPixelMap
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.stage.MessageStage
import com.promtuz.chat.ui.stage.rememberMessageStageState
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test

class MessageStageTest {
    @get:Rule val compose = createComposeRule()
    private var rows by mutableStateOf(emptyList<String>())

    private fun mount(initial: List<String> = emptyList()) {
        rows = initial
        compose.mainClock.autoAdvance = false
        compose.setContent {
            MessageStage(rows, { it }, rememberMessageStageState(), PaddingValues(0.dp),
                modifier = Modifier.size(160.dp, 200.dp).background(Color.Black),
                animateOnInitialFill = { it == "typing" },
                morphFrom = { if (it == "message") "typing" else null },
            ) { item ->
                Box(Modifier.size(80.dp, if (item == "message") 70.dp else 40.dp)
                    .background(if (item == "typing") Color.Red else Color.Blue))
            }
        }
        compose.mainClock.advanceTimeByFrame()
    }

    private fun coloredPixels(): Int {
        val pixels = compose.onRoot().captureToImage().toPixelMap()
        var count = 0
        for (y in 0 until pixels.height) for (x in 0 until pixels.width) {
            val color = pixels[x, y]
            if (color.red > 0.1f || color.blue > 0.1f) count++
        }
        return count
    }

    @Test fun typingAlreadyActiveOnOpenUnfoldsInsteadOfSnapping() {
        mount(listOf("typing"))
        val start = coloredPixels()
        compose.mainClock.advanceTimeBy(80)
        val middle = coloredPixels()
        compose.mainClock.advanceTimeBy(240)
        val end = coloredPixels()
        assertTrue("The bubble must grow during its entrance: $start, $middle, $end", start < middle && middle < end)
    }

    @Test fun historyOpensInPlaceButNewTypingStillAnimates() {
        mount(listOf("history"))
        val history = coloredPixels()
        compose.mainClock.advanceTimeBy(80)
        assertEquals(history, coloredPixels())
        compose.runOnIdle { rows = listOf("typing", "history") }
        compose.mainClock.advanceTimeByFrame()
        val start = coloredPixels()
        compose.mainClock.advanceTimeBy(80)
        val middle = coloredPixels()
        compose.mainClock.advanceTimeBy(240)
        val end = coloredPixels()
        assertTrue("Typing should grow beside settled history: $start, $middle, $end", start < middle && middle < end)
    }

    @Test fun typingCanReturnDuringExitWithoutBeingRemovedByTheOldExit() {
        mount(listOf("typing"))
        compose.mainClock.advanceTimeBy(300)
        val full = coloredPixels()
        compose.runOnIdle { rows = emptyList() }
        compose.mainClock.advanceTimeBy(80)
        val exiting = coloredPixels()
        assertTrue(exiting in 1 until full)
        compose.runOnIdle { rows = listOf("typing") }
        compose.mainClock.advanceTimeBy(300)
        assertEquals(full, coloredPixels())
        compose.runOnIdle { rows = emptyList() }
        compose.mainClock.advanceTimeBy(300)
        assertEquals(0, coloredPixels())
    }

    @Test fun messageCanInheritTypingThatAlreadyStartedExiting() {
        mount(listOf("typing"))
        compose.mainClock.advanceTimeBy(300)
        compose.runOnIdle { rows = emptyList() }
        compose.mainClock.advanceTimeBy(64)
        assertTrue(coloredPixels() > 0)
        compose.runOnIdle { rows = listOf("message") }
        compose.mainClock.advanceTimeByFrame()
        val start = coloredPixels()
        compose.mainClock.advanceTimeBy(80)
        val middle = coloredPixels()
        compose.mainClock.advanceTimeBy(240)
        val end = coloredPixels()
        assertTrue("The message should inherit room and grow: $start, $middle, $end", start > 0 && start < middle && middle < end)
    }
}
