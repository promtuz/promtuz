package com.promtuz.chat.ui

import android.graphics.Bitmap
import com.promtuz.chat.ui.theme.PromtuzTheme
import androidx.compose.runtime.*
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.test.espresso.Espresso.closeSoftKeyboard
import androidx.test.espresso.Espresso.pressBack
import androidx.test.platform.app.InstrumentationRegistry
import com.promtuz.chat.presentation.viewmodel.GroupWork
import com.promtuz.chat.presentation.viewmodel.UiMember
import com.promtuz.chat.ui.screens.GroupInfoActions
import com.promtuz.chat.ui.screens.GroupInfoContent
import com.promtuz.chat.ui.screens.GroupInfoState
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import java.io.File

/** Exercises real screen interactions with operation outcomes supplied at the core boundary. */
class GroupInfoTest {
    @get:Rule val compose = createComposeRule()
    private val alice = UiMember("alice", "Alice")
    private val bob = UiMember("bob", "Bob")
    private val me = UiMember("me", "You", admin = true, me = true)
    private fun initial() = GroupInfoState(listOf(me, alice), "Weekend plans", "Weekend plans", true,
        listOf(alice, bob, UiMember("cara", "Cara")), ownerIsStuck = true)
    private fun actions() = GroupInfoActions({}, {}, {}, {}, { _, _ -> }, {}, { _, _, _ -> }, { _, _ -> }, {}, {})

    @Test fun renameKeepsDraftOnFailureAndClosesOnlyOnSuccess() {
        var state by mutableStateOf(initial())
        var attempts = 0
        val actions = actions().copy(rename = { title, done ->
            attempts++
            if (attempts == 1) state = state.copy(work = GroupWork.Failed("Couldn’t save the name. Try again."))
            else { state = state.copy(title = title, displayName = title, work = GroupWork.Idle); done() }
        })
        compose.setContent { PromtuzTheme(darkTheme = true) { GroupInfoContent(state, actions) } }
        saveScreenshot("group-info.png")
        compose.onNodeWithText("Edit group name").performClick()
        compose.onNode(hasSetTextAction()).performTextReplacement("Summer trip")
        closeSoftKeyboard()
        compose.onNodeWithText("Save").performClick()
        compose.onNodeWithText("Couldn’t save the name. Try again.").assertIsDisplayed()
        compose.onNode(hasSetTextAction()).assertTextContains("Summer trip")
        compose.onNodeWithText("Save").performClick()
        compose.onNode(hasSetTextAction()).assertDoesNotExist()
        compose.onNodeWithText("Summer trip").assertIsDisplayed()
        compose.runOnIdle { assertEquals(2, attempts) }
    }

    @Test fun addingKeepsOnlyFailedSelectionsForRetry() {
        var state by mutableStateOf(initial().copy(members = listOf(me), candidates = listOf(alice, bob)))
        val requests = mutableListOf<List<String>>()
        val actions = actions().copy(addMembers = { people, added, done ->
            requests += people.map { it.ipkHex }
            if (requests.size == 1) {
                added("alice")
                state = state.copy(members = listOf(me, alice), work = GroupWork.Failed("1 added. Couldn’t add Bob. Try again."))
            } else {
                added("bob")
                state = state.copy(members = listOf(me, alice, bob), work = GroupWork.Idle)
                done()
            }
        })
        compose.setContent { PromtuzTheme(darkTheme = true) { GroupInfoContent(state, actions) } }
        compose.onNodeWithText("Add members").performClick()
        compose.onNodeWithText("Alice").performClick()
        compose.onNodeWithText("Bob").performClick()
        compose.onNodeWithText("Add 2 members").performClick()
        compose.onNodeWithText("1 added. Couldn’t add Bob. Try again.").assertIsDisplayed()
        compose.onNodeWithText("Add 1 member").performClick()
        compose.runOnIdle { assertEquals(listOf(listOf("alice", "bob"), listOf("bob")), requests) }
        compose.onNodeWithText("Search contacts").assertDoesNotExist()
    }

    @Test fun removingRequiresConfirmationAndCannotBeSubmittedTwiceWhileBusy() {
        var state by mutableStateOf(initial())
        var requests = 0
        val actions = actions().copy(removeMember = { _, _ ->
            requests++
            state = state.copy(work = GroupWork.Busy("Removing Alice…"))
        })
        compose.setContent { PromtuzTheme(darkTheme = true) { GroupInfoContent(state, actions) } }
        compose.onNodeWithText("Alice").performClick()
        compose.onNodeWithText("Remove from group").performClick()
        compose.runOnIdle { assertEquals(0, requests) }
        compose.onNodeWithText("Remove").performClick()
        compose.onNodeWithText("Removing Alice…").assertIsDisplayed()
        compose.onNodeWithText("Remove").assertIsNotEnabled()
        compose.runOnIdle { assertEquals(1, requests) }
    }

    @Test fun backClosesSearchBeforeTheMemberPicker() {
        compose.setContent { PromtuzTheme(darkTheme = true) { GroupInfoContent(initial(), actions()) } }
        compose.onNodeWithText("Add members").performClick()
        compose.onNodeWithContentDescription("Search contacts").performClick()
        compose.onNode(hasSetTextAction()).performTextInput("Bob")
        pressBack()
        compose.onNode(hasSetTextAction()).assertDoesNotExist()
        compose.onNodeWithText("New members won’t see earlier messages.").assertIsDisplayed()
        compose.onNodeWithContentDescription("Clear selection").performClick()
        compose.onNodeWithText("New members won’t see earlier messages.").assertDoesNotExist()
    }

    private fun saveScreenshot(name: String) {
        val screenshot = compose.onRoot().captureToImage().asAndroidBitmap()
        val directory = InstrumentationRegistry.getInstrumentation().targetContext.cacheDir
        File(directory, name).outputStream().use { screenshot.compress(Bitmap.CompressFormat.PNG, 100, it) }
    }
}
