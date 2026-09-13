package com.promtuz.chat.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.graphics.toPixelMap
import androidx.compose.ui.layout.layout
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.unit.*
import androidx.test.platform.app.InstrumentationRegistry
import com.promtuz.chat.domain.model.*
import com.promtuz.chat.ui.appearance.*
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.stage.*
import com.promtuz.chat.ui.theme.PromtuzTheme
import kotlinx.coroutines.CoroutineScope
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test

/** The production bubble and swipe wrapper, including their real multiline text layout. */
class ChatStackMotionTest {
    @get:Rule val compose = createComposeRule()

    @Test fun longOutgoingRisesFromBottomRightAndItsBoundsPushAndPullHistory() = exercise(6, 168)
    @Test fun singleLineKeepsTheSameBottomRightPivot() = exercise(1, 66)

    @Test fun singleLineSendingStatusDoesNotStartASecondGrowth() = exercise(1, 66, true)
    @Test fun multilineSendingStatusDoesNotStartASecondGrowth() = exercise(6, 168, true)

    private fun exercise(lines: Int, composerHeight: Int, changeStatus: Boolean = false) {
        fun message(key: String, outgoing: Boolean, text: String) = UiMessage(key, key, null,
            MessageContent.Text(text), outgoing, status = SendStatus.Sent, edited = false,
            deleted = false, timestampMs = 0, reactions = emptyList())
        val old = message("old", false, "Existing message")
        val fresh = message("new", true, (1..lines).joinToString("\n") { "Message line $it" })
            .let { if (changeStatus) it.copy(status = SendStatus.Pending) else it }
        var rows by mutableStateOf(listOf(old))
        val transition = SendTransition()
        lateinit var scope: CoroutineScope
        var pxPerDp = 1f
        var bodyMeasures = 0
        compose.mainClock.autoAdvance = false
        compose.setContent {
            scope = rememberCoroutineScope()
            pxPerDp = LocalDensity.current.density
            PromtuzTheme(darkTheme = true) {
                val colors = LocalChatColors.current.copy(outgoingBubble = Color.Red, incomingBubble = Color.Blue)
                val appearance = LocalChatAppearance.current.copy(bubble = BubbleStyle(cornerRadius = 0f, tail = false))
                CompositionLocalProvider(LocalChatColors provides colors, LocalChatAppearance provides appearance) {
                    val padding = remember {
                        object : PaddingValues {
                            override fun calculateTopPadding() = 0.dp
                            override fun calculateLeftPadding(layoutDirection: LayoutDirection) = 0.dp
                            override fun calculateRightPadding(layoutDirection: LayoutDirection) = 0.dp
                            override fun calculateBottomPadding() =
                                (composerHeight + (66 - composerHeight) * transition.progress.value).dp
                        }
                    }
                    MessageStage(rows, { it.key }, rememberMessageStageState(), padding,
                        Modifier.size(360.dp, 700.dp).background(Color.Black),
                        entranceClock = { if (it.key == "new") transition.claim() else null },
                        enterFromBelow = { if (it.key == "new") composerHeight * pxPerDp else 0f },
                        horizontalPivotInset = 12.dp,
                        transformOrigin = { TransformOrigin(if (it.outgoing) 1f else 0f, 1f) },
                    ) { msg ->
                        SwipeToReply(false, {}) {
                            MessageBubble(msg = msg, modifier = Modifier.layout { measurable, constraints ->
                                if (msg.key == "new") bodyMeasures++
                                val p = measurable.measure(constraints)
                                layout(p.width, p.height) { p.place(0, 0) }
                            })
                        }
                    }
                }
            }
        }
        compose.mainClock.advanceTimeBy(300)
        compose.runOnUiThread { transition.accept(scope) }
        compose.mainClock.advanceTimeBy(400)
        assertEquals("Cold message must not miss its entrance", 0f, transition.progress.value)
        compose.runOnUiThread { rows = listOf(fresh, old) }
        var last: Rect? = null
        repeat(7) { frame ->
            compose.mainClock.advanceTimeBy(32)
            val (red, blue) = bounds()
            if (red != null) {
                assertEquals("Bubble right edge must stay fixed", (360 - 12) * pxPerDp - 1, red.right, 2f)
                last?.let {
                    assertTrue("Bubble must rise, not pivot from its top: $it -> $red", red.bottom <= it.bottom + 2)
                    assertTrue("Bubble must grow from its right edge", red.left <= it.left + 2)
                }
                val anchor = (700 - composerHeight - (66 - composerHeight) * transition.progress.value) * pxPerDp
                val expectedOlderBottom = minOf(anchor, red.top)
                assertEquals("History must touch the growing bounds", expectedOlderBottom, blue!!.bottom + 1, 3f)
                last = red
            }
            if (frame == 3) capture("outgoing-${lines}-lines-middle")
        }
        assertNotNull(last)
        compose.mainClock.advanceTimeBy(300)
        val full = bounds().first!!
        assertEquals((700 - 66) * pxPerDp - 1, full.bottom, 2f)
        assertTrue("Stable text must not be measured on every animation frame: $bodyMeasures", bodyMeasures <= 3)
        if (changeStatus) {
            // The transport often completes after the entrance. Metadata must not
            // widen the bubble then, or move history onto another text/meta line.
            for (status in listOf(SendStatus.Sent, SendStatus.Failed, SendStatus.Pending)) {
                compose.runOnUiThread { rows = listOf(fresh.copy(status = status), old) }
                repeat(10) {
                    compose.mainClock.advanceTimeBy(32)
                    assertEquals("Status $status must preserve the settled bubble bounds", full, bounds().first)
                }
            }
        }
        compose.runOnUiThread { rows = listOf(old) }
        repeat(5) {
            compose.mainClock.advanceTimeBy(32)
            val (red, blue) = bounds()
            if (red != null) assertEquals("Removing a bubble must pull its neighbor with it", red.top, blue!!.bottom + 1, 3f)
        }
        compose.mainClock.advanceTimeBy(300)
        assertNull(bounds().first)
        assertEquals((700 - 66) * pxPerDp - 1, bounds().second!!.bottom, 2f)
    }

    private fun bounds(): Pair<Rect?, Rect?> {
        val pixels = compose.onRoot().captureToImage().toPixelMap()
        val limits = Array(2) { intArrayOf(pixels.width, pixels.height, -1, -1) }
        for (y in 0 until pixels.height) for (x in 0 until pixels.width) {
            val c = pixels[x, y]
            val i = when { c.red > 0.02f && c.green < 0.01f && c.blue < 0.01f -> 0
                c.blue > 0.2f && c.red < 0.01f && c.green < 0.01f -> 1
                else -> continue }
            val b = limits[i]
            b[0] = minOf(b[0], x); b[1] = minOf(b[1], y); b[2] = maxOf(b[2], x); b[3] = maxOf(b[3], y)
        }
        fun rect(i: Int) = limits[i].let { b -> if (b[2] < 0) null else Rect(b[0].toFloat(), b[1].toFloat(), b[2].toFloat(), b[3].toFloat()) }
        return rect(0) to rect(1)
    }

    private fun capture(name: String) {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        java.io.File(context.cacheDir, "$name.png").outputStream().use {
            compose.onRoot().captureToImage().asAndroidBitmap().compress(android.graphics.Bitmap.CompressFormat.PNG, 100, it)
        }
    }
}
