package com.promtuz.chat.ui

import androidx.compose.foundation.layout.*
import com.promtuz.chat.ui.components.ContactPickerHeader
import com.promtuz.chat.ui.theme.PromtuzTheme
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createComposeRule
import com.promtuz.chat.presentation.viewmodel.UiMember
import com.promtuz.chat.ui.components.ContactPicker
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test

class ContactPickerTest {
    @get:Rule val compose = createComposeRule()

    @Test fun contactsShowLivePresenceWithoutOpeningAChat() {
        var presence by mutableStateOf<com.promtuz.chat.domain.model.Presence>(com.promtuz.chat.domain.model.Presence.Online)
        compose.setContent {
            PromtuzTheme {
                com.promtuz.chat.ui.screens.ContactsContent(
                    com.promtuz.chat.ui.screens.ContactsState(
                        people = listOf(UiMember("alice", "Alice")), presence = mapOf("alice" to presence)),
                    com.promtuz.chat.ui.screens.ContactsActions({}, {}, {}, {}, {}, {}, { _, _ -> }, {}, {}),
                    {}, {},
                )
            }
        }
        compose.onNodeWithText("online").assertIsDisplayed()
        compose.runOnIdle { presence = com.promtuz.chat.domain.model.Presence.Idle(System.currentTimeMillis() - 120_000) }
        compose.onNode(hasText("idle since", substring = true)).assertIsDisplayed()
        compose.runOnIdle { presence = com.promtuz.chat.domain.model.Presence.LastSeen(System.currentTimeMillis() - 300_000) }
        compose.onNode(hasText("last seen", substring = true)).assertIsDisplayed()
        compose.runOnIdle { presence = com.promtuz.chat.domain.model.Presence.Unknown }
        compose.onNode(hasText("last seen", substring = true)).assertDoesNotExist()
        compose.onNodeWithText("Alice").assertIsDisplayed()
    }

    @Test fun selectionSurvivesSearchAndCanBeRemovedFromTheSelectedChips() {
        var picked by mutableStateOf(emptySet<String>())
        compose.setContent {
            var query by remember { mutableStateOf("") }
            PromtuzTheme(darkTheme = true) {
                Column(Modifier.fillMaxSize()) {
                var searching by remember { mutableStateOf(false) }
                ContactPickerHeader("Contacts", searching, query, { query = it }, true,
                    onBack = { searching = false; query = "" }, onSearch = { searching = true })
                ContactPicker(
                    people = listOf(UiMember("alice", "Alice"), UiMember("bob", "Bob")),
                    query = query, selected = picked, selecting = true, enabled = true,
                    onClick = { picked = if (it.ipkHex in picked) picked - it.ipkHex else picked + it.ipkHex },
                    modifier = Modifier.weight(1f),
                )
                }
            }
        }
        compose.onNodeWithText("Alice").performClick()
        compose.onNodeWithContentDescription("Search contacts").performClick()
        compose.onNode(hasSetTextAction()).performTextInput("Bob")
        compose.onNode(hasText("Bob") and hasClickAction() and !hasSetTextAction()).performClick()
        compose.runOnIdle { assertEquals(setOf("alice", "bob"), picked) }
        // Alice is filtered out of the list but remains reachable through her chip.
        compose.onNodeWithText("Alice").performClick()
        compose.runOnIdle { assertEquals(setOf("bob"), picked) }
        compose.onNode(hasSetTextAction()).performTextClearance()
        compose.onNodeWithText("Alice").assertIsNotSelected()
    }

    @Test fun longPressSelectsWithoutOpeningAChatAndBusyRowsIgnoreTaps() {
        var opened = 0
        var selected = 0
        var enabled by mutableStateOf(true)
        compose.setContent {
            PromtuzTheme(darkTheme = true) {
                ContactPicker(people = listOf(UiMember("alice", "Alice")), query = "", selected = emptySet(), selecting = false, enabled = enabled,
                    onClick = { opened++ }, onLongClick = { selected++ }, modifier = Modifier.fillMaxSize())
            }
        }
        compose.onNodeWithText("Alice").performTouchInput { longClick() }
        compose.runOnIdle { assertEquals(0, opened); assertEquals(1, selected); enabled = false }
        compose.onNodeWithText("Alice").performTouchInput { click() }
        compose.runOnIdle { assertEquals(0, opened) }
    }
}
