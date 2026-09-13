package com.promtuz.chat.ui

import android.graphics.Bitmap
import androidx.compose.runtime.*
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.test.platform.app.InstrumentationRegistry
import com.promtuz.chat.presentation.viewmodel.UiMember
import com.promtuz.chat.ui.screens.*
import com.promtuz.chat.ui.theme.PromtuzTheme
import org.junit.Rule
import org.junit.Test
import java.io.File

class ContactsScreenTest {
    @get:Rule val compose = createComposeRule()

    private fun mount() {
        compose.setContent {
            var state by remember { mutableStateOf(ContactsState(people = listOf(
                UiMember("alice", "Alice"), UiMember("bob", "Bob"), UiMember("cara", "Cara"),
                UiMember("daniel", "Daniel"), UiMember("ellie", "Ellie"), UiMember("frank", "Frank")))) }
            val actions = ContactsActions(
                clearPicks = { state = state.copy(picked = emptySet()) }, clearGroupError = {},
                togglePick = { key -> state = state.copy(picked = if (key in state.picked) state.picked - key else state.picked + key) },
                loadContacts = {}, open = {}, clearContactError = {}, delete = { _, _ -> }, create = {},
                setTitle = { state = state.copy(title = it) },
            )
            PromtuzTheme(darkTheme = true) { ContactsContent(state, actions, {}, {}) }
        }
    }

    @Test fun removingLastSelectionRestoresTheNormalToolbarAndActionRows() {
        mount()
        saveScreenshot("contacts-browse.png")
        compose.onNode(hasSetTextAction()).assertDoesNotExist()
        compose.onNodeWithText("Add Contact").assertIsDisplayed()
        compose.onNodeWithText("Alice").performTouchInput { longClick() }
        compose.onNodeWithText(" selected", substring = true).assertIsDisplayed()
        compose.onNodeWithContentDescription("Clear selection").assertIsDisplayed()
        saveScreenshot("contacts-selected.png")
        compose.onNodeWithContentDescription("Deselect Alice").performClick()
        compose.onNodeWithText("Contacts").assertIsDisplayed()
        compose.onNodeWithText("Add Contact").assertIsDisplayed()
        compose.onNodeWithContentDescription("Clear selection").assertDoesNotExist()
    }

    @Test fun creationUsesTheSharedNameDialogWithoutSearch() {
        mount()
        compose.onNodeWithText("Alice").performTouchInput { longClick() }
        compose.onNodeWithContentDescription("Search contacts").assertDoesNotExist()
        compose.onNodeWithText("Create group").performClick()
        compose.onNode(hasSetTextAction()).performTextInput("Weekend plans")
        saveScreenshot("group-name.png", dialog = true)
        compose.onNodeWithText("Cancel").performClick()
        compose.onNode(hasSetTextAction()).assertDoesNotExist()
        compose.onNodeWithText(" selected", substring = true).assertIsDisplayed()
        compose.onNodeWithContentDescription("Clear selection").performClick()
        compose.onNodeWithText("Contacts").assertIsDisplayed()
    }

    @Test fun fastFlingDuringSelectionDoesNotLeaveAScrollGap() {
        mount()
        compose.mainClock.autoAdvance = false
        compose.onNodeWithText("Alice").performTouchInput { longClick() }
        compose.mainClock.advanceTimeByFrame()
        compose.onAllNodes(hasScrollAction())[1].performTouchInput { swipeUp(durationMillis = 60) }
        compose.mainClock.autoAdvance = true
        compose.waitForIdle()
        compose.onNodeWithText("Frank").assertIsDisplayed()
        compose.onAllNodesWithText("Alice").filter(hasAnyAncestor(hasScrollAction())).assertCountEquals(2)
        compose.onNodeWithText("Share").assertDoesNotExist()
        compose.onNodeWithContentDescription("Delete contact").assertIsDisplayed()
    }

    @Test fun changingTheCountKeepsTheSelectedLabelInPlace() {
        mount()
        compose.onNodeWithText("Alice").performTouchInput { longClick() }
        val before = compose.onNodeWithText(" selected", useUnmergedTree = true).fetchSemanticsNode().boundsInRoot
        compose.onNodeWithText("Bob").performClick()
        val after = compose.onNodeWithText(" selected", useUnmergedTree = true).fetchSemanticsNode().boundsInRoot
        org.junit.Assert.assertEquals(before.top, after.top, 0.5f)
        compose.onNodeWithText("2", useUnmergedTree = true).assertIsDisplayed()
        compose.onNodeWithContentDescription("Delete contact").assertDoesNotExist()
    }

    private fun saveScreenshot(name: String, dialog: Boolean = false) {
        val node = if (dialog) compose.onNode(isDialog()) else compose.onRoot()
        val bitmap = node.captureToImage().asAndroidBitmap()
        val directory = InstrumentationRegistry.getInstrumentation().targetContext.cacheDir
        File(directory, name).outputStream().use { bitmap.compress(Bitmap.CompressFormat.PNG, 100, it) }
    }
}
