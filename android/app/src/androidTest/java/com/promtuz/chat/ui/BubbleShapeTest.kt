package com.promtuz.chat.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toPixelMap
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.appearance.BubbleStyle
import com.promtuz.chat.ui.components.TypingBubble
import com.promtuz.chat.ui.components.rememberBubbleShape
import com.promtuz.chat.ui.theme.PromtuzTheme
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test

class BubbleShapeTest {
    @get:Rule val compose = createComposeRule()

    @Test fun tailRetractsAndReturnsOverMultipleFrames() {
        var merged by mutableStateOf(false)
        var bodyLeft = 0
        compose.mainClock.autoAdvance = false
        compose.setContent {
            bodyLeft = with(LocalDensity.current) { 16.dp.roundToPx() }
            val shape = rememberBubbleShape(false, false, merged, BubbleStyle())
            Box(Modifier.size(120.dp, 80.dp).background(Color.Black).padding(16.dp)) {
                Box(Modifier.size(80.dp, 40.dp).background(Color.Red, shape))
            }
        }
        fun tailPixels(): Int {
            val pixels = compose.onRoot().captureToImage().toPixelMap()
            return (0 until pixels.height).sumOf { y -> (0 until bodyLeft).count { x -> pixels[x, y].red > 0.2f } }
        }
        compose.mainClock.advanceTimeByFrame()
        val full = tailPixels()
        assertTrue(full > 0)
        compose.runOnIdle { merged = true }
        compose.mainClock.advanceTimeBy(80)
        val middle = tailPixels()
        compose.mainClock.advanceTimeBy(240)
        assertTrue("Tail should shrink smoothly: $full -> $middle", middle in 1 until full)
        assertEquals(0, tailPixels())
        compose.runOnIdle { merged = false }
        compose.mainClock.advanceTimeBy(300)
        assertEquals(full, tailPixels())
    }

    @Test fun typingBubblePaintsItsTailOutsideTheBody() {
        var bodyLeft = 0
        compose.mainClock.autoAdvance = false
        compose.setContent {
            bodyLeft = with(LocalDensity.current) { 12.dp.roundToPx() }
            PromtuzTheme(darkTheme = true) {
                Box(Modifier.size(160.dp, 70.dp).background(Color.Black)) { TypingBubble() }
            }
        }
        compose.mainClock.advanceTimeByFrame()
        val pixels = compose.onRoot().captureToImage().toPixelMap()
        val visibleTail = (0 until pixels.height).any { y ->
            (0 until bodyLeft - 1).any { x -> pixels[x, y].red > 0.04f }
        }
        assertTrue("The tail must not be clipped at the body’s left edge", visibleTail)
    }
}
