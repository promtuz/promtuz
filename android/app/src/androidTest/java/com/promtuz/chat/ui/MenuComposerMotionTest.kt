package com.promtuz.chat.ui

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.toPixelMap
import androidx.compose.ui.layout.boundsInRoot
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.unit.dp
import com.promtuz.chat.domain.model.*
import com.promtuz.chat.ui.appearance.*
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.stage.*
import com.promtuz.chat.ui.theme.PromtuzTheme
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test

class MenuComposerMotionTest {
    @get:Rule val compose = createComposeRule()

    @Test fun closingLiftedMessageTracksGrowingReplyAreaUntilTheRealRowReturns() = exercise(false)
    @Test fun editingMultilineTextAlsoTracksFieldGrowthAtTheBottomOfHistory() = exercise(true)

    private fun exercise(growField: Boolean) {
        val menu = MessageMenuState()
        var replying by mutableStateOf(false)
        var bounds = Rect.Zero
        var totalGrowthPx = 0f
        val message = UiMessage("one", "one", null, MessageContent.Text("Reply to this"), true,
            status = SendStatus.Sent, edited = false, deleted = false, timestampMs = 0, reactions = emptyList())
        compose.mainClock.autoAdvance = false
        compose.setContent {
            val push by animateFloatAsState(if (replying) 64f else 0f, tween(160))
            val fieldGrowth = if (growField) push * 1.5f else 0f
            totalGrowthPx = with(LocalDensity.current) { (push + fieldGrowth).dp.toPx() }
            val stage = rememberMessageStageState()
            LaunchedEffect(menu.anchor) {
                menu.anchor?.let { stage.pin("one", it.bounds.bottom) } ?: stage.unpin()
            }
            PromtuzTheme {
                CompositionLocalProvider(
                    LocalChatColors provides LocalChatColors.current.copy(outgoingBubble = Color.Red),
                    LocalChatAppearance provides LocalChatAppearance.current.copy(bubble = BubbleStyle(cornerRadius = 0f, tail = false)),
                ) {
                    Box(Modifier.size(360.dp, 700.dp).background(Color.Black)) {
                        MessageStage(listOf(message), { it.key }, stage, PaddingValues(bottom = (66 + push + fieldGrowth).dp),
                            Modifier.fillMaxSize(), pushBottom = { push.dp }) { msg ->
                            MessageBubble(msg = msg, modifier = Modifier.onGloballyPositioned { bounds = it.boundsInRoot() }
                                .graphicsLayer { alpha = if (menu.isOpen) 0f else 1f })
                        }
                        menu.anchor?.let { anchor ->
                            MessageContextMenu(menu, listOf("👍"), listOf(listOf(
                                MenuAction(if (growField) "Edit" else "Reply") { replying = true; menu.close() },
                            )),
                                anchorOffsetY = { stage.pinnedOffsetY }, onReact = {})
                        }
                    }
                }
            }
        }
        compose.mainClock.advanceTimeBy(350)
        val originalBottom = redBottom()
        compose.runOnUiThread { menu.open(MenuAnchor(message, bounds, false, false)) }
        compose.mainClock.advanceTimeBy(400)
        compose.onNodeWithText(if (growField) "Edit" else "Reply").performClick()
        compose.mainClock.advanceTimeBy(144)
        val beforeRelease = redBottom()
        assertTrue("The lifted copy must follow the growing composer before it is released",
            beforeRelease < originalBottom - totalGrowthPx * 0.8f)
        compose.mainClock.advanceTimeBy(80)
        val afterRelease = redBottom()
        assertEquals("The real message must continue from the lifted copy's position",
            originalBottom - totalGrowthPx, afterRelease.toFloat(), 2f)
        assertTrue("Releasing the menu must not jump back to the old baseline",
            kotlin.math.abs(beforeRelease - afterRelease) < 24)
    }

    private fun redBottom(): Int {
        val pixels = compose.onRoot().captureToImage().toPixelMap()
        for (y in pixels.height - 1 downTo 0) for (x in 0 until pixels.width) {
            val color = pixels[x, y]
            if (color.red > 0.8f && color.green < 0.05f && color.blue < 0.05f) return y
        }
        error("Message not visible")
    }
}
