package com.promtuz.chat.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.*
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toPixelMap
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.navigation3.runtime.*
import com.promtuz.chat.navigation.*
import com.promtuz.chat.ui.theme.PromtuzTheme
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test

class NavReplacementTest {
    @get:Rule val compose = createComposeRule()

    @Test fun replacingContactsSlidesChatOverContactsAndBackReturnsHome() {
        lateinit var stack: NavBackStack<androidx.navigation3.runtime.NavKey>
        compose.mainClock.autoAdvance = false
        compose.setContent {
            stack = rememberNavBackStack(Routes.App, Routes.Contacts)
            PromtuzTheme {
                NavStage(stack, { stack.removeLastOrNull() }) { route ->
                    NavEntry(route) {
                        Box(Modifier.fillMaxSize().background(when (route) {
                            Routes.Contacts -> Color.Blue
                            Routes.App -> Color.Green
                            else -> Color.Red
                        }).testTag(route.toString()))
                    }
                }
            }
        }
        compose.mainClock.advanceTimeBy(350)
        compose.runOnUiThread { stack[stack.lastIndex] = Routes.Chat("test", "Test") }
        compose.mainClock.advanceTimeBy(80)
        val pixels = compose.onRoot().captureToImage().toPixelMap()
        val y = pixels.height / 2
        assertTrue("Replaced Contacts must remain behind the moving chat", pixels[pixels.width / 20, y].blue > 0.9f)
        assertTrue("Chat must slide into view", pixels[pixels.width * 19 / 20, y].red > 0.9f)
        compose.mainClock.advanceTimeBy(500)
        assertEquals(listOf(Routes.App, Routes.Chat("test", "Test")), stack.toList())
        compose.runOnUiThread { stack.removeLastOrNull() }
        compose.mainClock.advanceTimeBy(500)
        assertEquals(listOf(Routes.App), stack.toList())
        compose.onNodeWithTag(Routes.App.toString()).assertIsDisplayed()
    }
}
