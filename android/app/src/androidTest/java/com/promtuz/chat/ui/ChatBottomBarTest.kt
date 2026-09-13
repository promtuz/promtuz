package com.promtuz.chat.ui

import android.app.Application
import org.junit.Assert.*
import androidx.compose.foundation.layout.*
import androidx.compose.material3.Scaffold
import androidx.compose.runtime.*
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.unit.dp
import androidx.test.platform.app.InstrumentationRegistry
import com.promtuz.chat.domain.model.*
import com.promtuz.chat.presentation.viewmodel.*
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.theme.PromtuzTheme
import dev.chrisbanes.haze.HazeState
import org.junit.Rule
import org.junit.Test
import org.koin.core.context.GlobalContext

/** Real composer and VM transitions. No conversation initialization or real messages. */
class ChatBottomBarTest {
    @get:Rule val compose = createComposeRule()
    @Test fun editSessionsRestoreDraftAndReplyAndKeepUnfinishedEdits() {
        val application = InstrumentationRegistry.getInstrumentation().targetContext.applicationContext as Application
        compose.runOnUiThread {
            val vm = ChatVM(application, GlobalContext.get().get<AppVM>())
            fun message(key: String) = UiMessage(key, key, null, MessageContent.Text(key),
                true, status = SendStatus.Sent, edited = false, deleted = false, timestampMs = 0, reactions = emptyList())
            vm.input.value = "Unsent draft"
            vm.beginReply(message("reply target"))
            vm.beginEdit(message("A"))
            vm.input.value = "Unfinished A"
            vm.beginEdit(message("B"))
            vm.beginEdit(message("A"))
            assertEquals("Unfinished A", vm.input.value)
            vm.cancelComposerAction()
            assertEquals("Unsent draft", vm.input.value)
            assertEquals("reply target", vm.composerAction.value?.msg?.key)
            vm.beginEdit(message("A"))
            vm.beginReply(message("new reply"))
            assertEquals("Unsent draft", vm.input.value)
            assertEquals("new reply", vm.composerAction.value?.msg?.key)
        }
    }

    @Test fun rejectedEditKeepsItsTextAndTarget() {
        val application = InstrumentationRegistry.getInstrumentation().targetContext.applicationContext as Application
        lateinit var vm: ChatVM
        compose.runOnUiThread {
            vm = ChatVM(application, GlobalContext.get().get<AppVM>())
            val missing = UiMessage("missing", "missing", "00".repeat(16), MessageContent.Text("old"),
                true, status = SendStatus.Sent, edited = false, deleted = false, timestampMs = 0, reactions = emptyList())
            vm.beginEdit(missing)
            vm.input.value = "Keep this edit"
            vm.send()
        }
        compose.waitUntil { !vm.composerBusy.value }
        compose.runOnUiThread {
            assertEquals("Keep this edit", vm.input.value)
            assertEquals("missing", vm.composerAction.value?.msg?.key)
            assertNotNull(vm.composerError.value)
        }
    }

    @OptIn(ExperimentalLayoutApi::class)
    @Test fun keyboardAndAttachmentPanelReserveTheirActualHeight() {
        val application = InstrumentationRegistry.getInstrumentation().targetContext.applicationContext as Application
        lateinit var vm: ChatVM
        val metrics = ComposerMetrics()
        var imeBottom = 0
        var imeVisible = false
        compose.runOnUiThread { vm = ChatVM(application, GlobalContext.get().get<AppVM>()) }
        compose.setContent {
            imeVisible = WindowInsets.isImeVisible
            imeBottom = WindowInsets.ime.getBottom(LocalDensity.current)
            PromtuzTheme(darkTheme = true) {
                Scaffold(bottomBar = {
                    Box(Modifier.testTag("bar")) { ChatBottomBar(vm, remember { HazeState() }, metrics) }
                }) { padding -> Box(Modifier.padding(padding)) }
            }
        }
        fun verify() {
            val bar = compose.onNodeWithTag("bar").fetchSemanticsNode().boundsInRoot
            assertEquals(bar.height, metrics.bottomPx, 1f)
            compose.onNodeWithContentDescription("Message input").assertIsDisplayed()
        }
        compose.onNodeWithContentDescription("Message input").performClick()
        compose.waitUntil(5000) { imeVisible }
        val keyboardHeight = imeBottom
        verify()
        compose.runOnUiThread { vm.composerBusy.value = true }
        compose.waitForIdle()
        compose.onNodeWithContentDescription("Message input").assertIsFocused()
        assertTrue("Sending must retain IME", imeVisible)
        compose.runOnUiThread { vm.composerBusy.value = false }
        compose.onNode(hasContentDescription("Attach media") and isEnabled()).performClick()
        compose.waitUntil(5000) { !imeVisible }
        verify()
        compose.onNode(hasContentDescription("Attach media") and isEnabled()).performClick()
        compose.waitUntil(5000) { imeVisible && imeBottom >= keyboardHeight }
        compose.onNodeWithContentDescription("Message input").assertIsFocused()
        verify()
        compose.onNode(hasContentDescription("Attach media") and isEnabled()).performClick()
        compose.waitUntil(5000) { !imeVisible }
        compose.onNodeWithContentDescription("Message input").performClick()
        compose.waitForIdle()
        verify()
    }

    @OptIn(ExperimentalLayoutApi::class)
    @Test fun editFromLiftedMenuWithKeyboardOpenKeepsComposerSlotsSeparate() {
        val application = InstrumentationRegistry.getInstrumentation().targetContext.applicationContext as Application
        lateinit var vm: ChatVM
        var imeVisible = false
        var imeHeight = 0
        val msg = UiMessage("target", "target", "11".repeat(16), MessageContent.Text("dfgsdfgsdfg"),
            true, status = SendStatus.Sent, edited = false, deleted = false, timestampMs = 0, reactions = emptyList())
        compose.runOnUiThread {
            vm = ChatVM(application, GlobalContext.get().get<AppVM>())
            // Seed presentation state only. No database writes or live peer traffic.
            val field = ChatVM::class.java.getDeclaredField("_messages").apply { isAccessible = true }
            @Suppress("UNCHECKED_CAST")
            val messages = field.get(vm) as kotlinx.coroutines.flow.MutableStateFlow<List<UiMessage>>
            messages.value = listOf(
                msg.copy(key = "newest", dispatchIdHex = "22".repeat(16), content = MessageContent.Text("hkjh"), timestampMs = 120_000),
                msg,
                msg.copy(key = "older", dispatchIdHex = "33".repeat(16), outgoing = false, content = MessageContent.Text("hggv")),
            )
        }
        compose.setContent {
            imeVisible = WindowInsets.isImeVisible
            imeHeight = WindowInsets.ime.getBottom(LocalDensity.current)
            PromtuzTheme(darkTheme = true) {
                com.promtuz.chat.ui.screens.ChatScreen("Reproduction", vm)
            }
        }
        compose.onNodeWithContentDescription("Message input").performClick()
        compose.waitUntil(5000) { imeVisible }
        compose.waitForIdle()
        assertTrue("Reproduction requires a docked keyboard, not a zero-inset floating IME", imeHeight > 0)
        compose.onNodeWithText("dfgsdfgsdfg").performTouchInput { longClick() }
        compose.onNodeWithText("Edit").assertIsDisplayed()
        compose.mainClock.autoAdvance = false
        compose.onNodeWithText("Edit").performClick()
        repeat(18) { frame ->
            compose.mainClock.advanceTimeBy(32)
            if (frame == 3 || frame == 17) {
                java.io.File(application.cacheDir, "composer-ime-edit-$frame.png").outputStream().use {
                    compose.onRoot().captureToImage().asAndroidBitmap()
                        .compress(android.graphics.Bitmap.CompressFormat.PNG, 100, it)
                }
            }
        }
        val field = compose.onNodeWithContentDescription("Message input").fetchSemanticsNode().boundsInRoot
        val hint = compose.onNodeWithText("Tap to add media").fetchSemanticsNode().boundsInRoot
        assertTrue("Edit header overlaps input with IME open: $hint / $field", hint.bottom <= field.top + 1)
        assertTrue("Editing should retain the IME", imeVisible)
    }

    @Test fun interruptedReplyEditAndHeightTransitions() {
        val application = InstrumentationRegistry.getInstrumentation().targetContext.applicationContext as Application
        lateinit var vm: ChatVM
        val metrics = ComposerMetrics()
        var height by mutableStateOf(620)
        compose.runOnUiThread { vm = ChatVM(application, GlobalContext.get().get<AppVM>()) }
        compose.mainClock.autoAdvance = false
        compose.setContent {
            PromtuzTheme(darkTheme = true) {
                Box(Modifier.fillMaxWidth().height(height.dp)) {
                    Scaffold(bottomBar = {
                        Box(Modifier.testTag("bar")) { ChatBottomBar(vm, remember { HazeState() }, metrics) }
                    }) { padding -> Box(Modifier.padding(padding)) }
                }
            }
        }
        fun message(text: String) = UiMessage(text.hashCode().toString(), "audit", null, MessageContent.Text(text),
            true, status = SendStatus.Sent, edited = false, deleted = false, timestampMs = 0, reactions = emptyList())
        val long = (1..12).joinToString("\n") { "A long draft line $it" }
        fun sample(label: String) {
            compose.mainClock.advanceTimeBy(32)
            val field = compose.onNode(hasSetTextAction()).fetchSemanticsNode().boundsInRoot
            val bar = compose.onNodeWithTag("bar").fetchSemanticsNode().boundsInRoot
            assertEquals("$label: stage and bar must agree", bar.height, metrics.bottomPx, 1f)
            assertTrue("$label: input disappeared", field.height > 0)
            assertTrue("$label: input overlaps bottom region", field.bottom <= bar.bottom - metrics.regionPx + 1)
            assertTrue("$label: input is above bar", field.top >= bar.top - 1)
        }
        sample("initial")
        repeat(4) {
            compose.runOnUiThread { vm.input.value = long; vm.beginReply(message("Short reply")) }
            sample("long+reply")
            compose.runOnUiThread { vm.beginEdit(message("x")) }
            sample("short edit")
            compose.runOnUiThread { vm.beginEdit(message(long)); height = 240 }
            sample("long edit small viewport")
            compose.runOnUiThread { vm.cancelComposerAction() }
            sample("cancel")
            compose.runOnUiThread { vm.beginReply(message(long)); vm.input.value = "x"; height = 620 }
            sample("reply while exit")
        }
        compose.mainClock.advanceTimeBy(1000)
        sample("settled")
        compose.runOnUiThread { height = 120; vm.beginEdit(message(long)) }
        compose.mainClock.advanceTimeBy(1000)
        sample("severely constrained")
        val bitmap = compose.onRoot().captureToImage().asAndroidBitmap()
        java.io.File(application.cacheDir, "composer-constrained.png").outputStream().use {
            bitmap.compress(android.graphics.Bitmap.CompressFormat.PNG, 100, it)
        }
    }
}
