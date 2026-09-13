package com.promtuz.chat.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.graphics.toPixelMap
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.test.platform.app.InstrumentationRegistry
import com.promtuz.chat.domain.model.*
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.stage.*
import com.promtuz.chat.ui.theme.PromtuzTheme
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test

class BubbleEntranceTest {
    @get:Rule val compose = createComposeRule()

    private fun message() = UiMessage("message", "message", null,
        MessageContent.Text((1..8).joinToString("\n") { "A longer incoming message, line $it" }),
        false, status = SendStatus.Sent, edited = false, deleted = false, timestampMs = 0, reactions = emptyList())

    private fun capture(name: String) {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        java.io.File(context.cacheDir, "$name.png").outputStream().use {
            compose.onRoot().captureToImage().asAndroidBitmap().compress(android.graphics.Bitmap.CompressFormat.PNG, 100, it)
        }
    }

    @Test fun typingSurfaceGrowsIntoMultilineMessage() {
        var rows by mutableStateOf(listOf("typing"))
        compose.mainClock.autoAdvance = false
        compose.setContent {
            PromtuzTheme(darkTheme = true) {
                MessageStage(rows, { it }, rememberMessageStageState(), PaddingValues(0.dp),
                    Modifier.size(360.dp, 500.dp).background(Color.Black),
                    morphFrom = { if (it == "message") "typing" else null },
                ) { if (it == "typing") TypingBubble() else MessageBubble(msg = message()) }
            }
        }
        compose.mainClock.advanceTimeBy(300)
        fun surfaceBounds(): androidx.compose.ui.geometry.Rect {
            val pixels = compose.onRoot().captureToImage().toPixelMap()
            var left = pixels.width; var top = pixels.height; var right = 0; var bottom = 0
            for (y in 0 until pixels.height) for (x in 0 until pixels.width) {
                val c = pixels[x, y]
                if (c.red + c.green + c.blue > 0.1f) {
                    left = minOf(left, x); top = minOf(top, y); right = maxOf(right, x); bottom = maxOf(bottom, y)
                }
            }
            return androidx.compose.ui.geometry.Rect(left.toFloat(), top.toFloat(), right.toFloat(), bottom.toFloat())
        }
        val typing = surfaceBounds()
        capture("bubble-typing")
        compose.runOnUiThread { rows = listOf("message") }
        compose.mainClock.advanceTimeByFrame()
        val start = surfaceBounds()
        capture("bubble-morph-start")
        assertEquals("Surface must inherit typing width", typing.width, start.width, 3f)
        assertEquals("Surface must inherit typing height", typing.height, start.height, 3f)
        compose.mainClock.advanceTimeBy(80)
        val middle = surfaceBounds()
        capture("bubble-morph-middle")
        compose.mainClock.advanceTimeBy(240)
        val end = surfaceBounds()
        capture("bubble-morph-end")
        assertEquals(typing.bottom, end.bottom, 1f)
        assertTrue(start.width < middle.width && middle.width < end.width)
        assertTrue(start.height < middle.height && middle.height < end.height)
        compose.runOnUiThread { rows = emptyList() }
        compose.mainClock.advanceTimeBy(96)
        val shrinking = surfaceBounds()
        assertTrue("A completed handoff must shrink as one message", shrinking.height < end.height)
        assertEquals("Deleting a morphed message must retain its aspect ratio",
            end.width / end.height, shrinking.width / shrinking.height, 0.04f)
        compose.mainClock.advanceTimeBy(300)
    }

    @Test fun expiredTypingUsesUndistortedWholeRowEntrance() {
        var rows by mutableStateOf(listOf("history", "typing"))
        compose.mainClock.autoAdvance = false
        compose.setContent {
            MessageStage(rows, { it }, rememberMessageStageState(), PaddingValues(0.dp),
                Modifier.size(320.dp, 500.dp).background(Color.Black),
                morphFrom = { if (it == "message") "typing" else null },
            ) {
                Box(Modifier.size(100.dp, if (it == "message") 300.dp else 20.dp)
                    .background(if (it == "message") Color.Red else Color.Blue))
            }
        }
        compose.mainClock.advanceTimeBy(300)
        compose.runOnUiThread { rows = listOf("history") }
        compose.mainClock.advanceTimeBy(300)
        compose.runOnUiThread { rows = listOf("message", "history") }
        compose.mainClock.advanceTimeBy(80)
        val pixels = compose.onRoot().captureToImage().toPixelMap()
        var left = pixels.width; var top = pixels.height; var right = 0; var bottom = 0
        for (y in 0 until pixels.height) for (x in 0 until pixels.width) if (pixels[x, y].red > 0.1f) {
            left = minOf(left, x); top = minOf(top, y); right = maxOf(right, x); bottom = maxOf(bottom, y)
        }
        assertTrue(right > left)
        assertEquals("Whole row must retain its aspect ratio", 3f, (bottom - top).toFloat() / (right - left), 0.08f)
    }

    @Test fun acceptedMultilineTextFadesAndCanBeInterruptedByNextDraft() {
        var text by mutableStateOf((1..6).joinToString("\n") { "Draft line $it" })
        var revision by mutableLongStateOf(0)
        lateinit var textExit: ComposerTextExit
        compose.mainClock.autoAdvance = false
        compose.setContent {
            textExit = rememberComposerTextExit(revision, text) { it.measured() }
            Box(Modifier.size(320.dp, 300.dp).background(Color.Black)) {
                BasicTextField(text, { text = it },
                    Modifier.align(androidx.compose.ui.Alignment.BottomStart).fillMaxWidth()
                        .then(textExit.modifier).testTag("input"),
                    textStyle = TextStyle(color = Color.White, fontSize = 20.sp), maxLines = 6)
            }
        }
        compose.mainClock.advanceTimeBy(300)
        fun ink(): Int {
            val pixels = compose.onRoot().captureToImage().toPixelMap()
            var count = 0
            for (y in 0 until pixels.height) for (x in 0 until pixels.width) if (pixels[x, y].red > 0.1f) count++
            return count
        }
        val full = ink()
        compose.runOnUiThread { textExit.submit { revision++; text = "" } }
        compose.waitUntil { !textExit.capturing }
        compose.mainClock.advanceTimeBy(64)
        val fading = ink()
        capture("composer-send-middle")
        assertTrue("Sent text must remain visible while fading: $full -> $fading", fading > 0 && fading < full)
        compose.runOnUiThread { text = "Next message" }
        compose.mainClock.advanceTimeBy(300)
        compose.onNodeWithTag("input").assertTextContains("Next message")
        compose.runOnUiThread { textExit.submit { revision++; text = "" } }
        compose.waitUntil { !textExit.capturing }
        compose.mainClock.advanceTimeBy(300)
        assertEquals("Sent text must finish disappearing", 0, ink())
    }
    @Test fun delayedBubbleSharesComposerClockAndNextSendDoesNotRestartIt() {
        var text by mutableStateOf("First draft")
        var revision by mutableLongStateOf(0)
        var rows by mutableStateOf(emptyList<String>())
        var transaction: SendTransition? = null
        lateinit var exit: ComposerTextExit
        compose.mainClock.autoAdvance = false
        compose.setContent {
            exit = rememberComposerTextExit(revision, text) { transaction = it }
            Column(Modifier.size(320.dp, 600.dp).background(Color.Black)) {
                MessageStage(rows, { it }, rememberMessageStageState(), PaddingValues(0.dp),
                    Modifier.fillMaxWidth().weight(1f), entranceClock = { transaction?.claim() },
                ) { key ->
                    Box(Modifier.size(100.dp, 120.dp).background(if (key == "first") Color.Red else Color.Blue))
                }
                BasicTextField(text, { text = it }, Modifier.fillMaxWidth().then(exit.modifier),
                    textStyle = TextStyle(color = Color.White, fontSize = 24.sp))
            }
        }
        compose.mainClock.advanceTimeBy(300)
        compose.runOnUiThread { exit.submit { revision++; text = "" } }
        compose.waitUntil { !exit.capturing }
        compose.mainClock.advanceTimeBy(400) // Longer than a full entrance: keep the handoff until measured.
        compose.runOnUiThread { rows = listOf("first") }
        compose.mainClock.advanceTimeBy(48)
        fun redAndWhite(): Pair<Float, Float> {
            val pixels = compose.onRoot().captureToImage().toPixelMap()
            var red = 0f; var white = 0f
            for (y in 0 until pixels.height) for (x in 0 until pixels.width) {
                val c = pixels[x, y]
                if (c.green < 0.02f && c.blue < 0.02f) red = maxOf(red, c.red)
                if (kotlin.math.abs(c.red - c.green) < 0.02f) white = maxOf(white, c.green)
            }
            return red to white
        }
        val (bubble, composer) = redAndWhite()
        assertTrue(bubble > 0f && composer > 0f)
        assertEquals("Delayed row and disappearing draft must share progress", 1f, bubble + composer, 0.06f)
        compose.runOnUiThread { text = "Second draft" }
        compose.mainClock.advanceTimeByFrame()
        compose.runOnUiThread { exit.submit { revision++; text = ""; rows = listOf("second", "first") } }
        compose.waitUntil { !exit.capturing }
        compose.mainClock.advanceTimeBy(48)
        assertTrue("The first send must keep progressing", redAndWhite().first > bubble)
        compose.mainClock.advanceTimeBy(300)
        assertEquals(1f, redAndWhite().first, 0.01f)
    }

}
