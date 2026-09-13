package com.promtuz.chat.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toPixelMap
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.components.*
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test

class MorphIconTest {
    @get:Rule val compose = createComposeRule()

    @Test fun rapidRetargetingKeepsCurrentShapeAndSettlesAtTheNewIcon() {
        var glyph by mutableStateOf(MorphGlyph.Back)
        compose.mainClock.autoAdvance = false
        compose.setContent {
            MorphIcon(glyph, glyph.name, Modifier.size(48.dp).background(Color.Black), Color.White)
        }
        compose.mainClock.advanceTimeBy(300)
        val back = pixels()
        compose.runOnUiThread { glyph = MorphGlyph.Close }
        compose.mainClock.advanceTimeBy(80)
        val middle = pixels()
        assertFalse(back.contentEquals(middle))
        compose.runOnUiThread { glyph = MorphGlyph.Back }
        // Retargeting must not snap to either endpoint before its first frame.
        assertTrue(middle.contentEquals(pixels()))
        compose.mainClock.advanceTimeBy(300)
        assertTrue(back.contentEquals(pixels()))
        compose.runOnUiThread { glyph = MorphGlyph.Pause }
        compose.mainClock.advanceTimeBy(300)
        compose.onNodeWithContentDescription("Pause").assertExists()
        assertFalse(back.contentEquals(pixels()))
    }

    private fun pixels(): IntArray {
        val map = compose.onRoot().captureToImage().toPixelMap()
        return IntArray(map.width * map.height) { i -> (map[i % map.width, i / map.width].red * 255).toInt() }
    }
}
