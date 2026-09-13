package com.promtuz.chat.ui

import android.app.Application
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.test.platform.app.InstrumentationRegistry
import com.promtuz.chat.presentation.viewmodel.*
import com.promtuz.chat.ui.screens.ChatScreen
import com.promtuz.chat.ui.theme.PromtuzTheme
import org.junit.Rule
import org.junit.Test
import org.koin.core.context.GlobalContext

class ChatSearchBarTest {
    @get:Rule val compose = createComposeRule()

    @Test fun searchMenuKeepsToolbarVisibleAndCloseRestoresCompactBack() {
        val app = InstrumentationRegistry.getInstrumentation().targetContext.applicationContext as Application
        lateinit var vm: ChatVM
        compose.runOnUiThread { vm = ChatVM(app, GlobalContext.get().get<AppVM>()) }
        compose.setContent { PromtuzTheme(darkTheme = true) { ChatScreen("Search fixture", vm) } }
        repeat(3) {
            compose.onNodeWithContentDescription("Chat options").performClick()
            compose.onNodeWithText("Search").performClick()
            compose.onNodeWithContentDescription("Search messages").assertIsDisplayed().assertIsFocused()
            compose.onNodeWithContentDescription("Close search").assertIsDisplayed()
            compose.onNodeWithContentDescription("Search messages").performTextInput("hello")
            if (it == 0) java.io.File(app.cacheDir, "chat-search-toolbar.png").outputStream().use { output ->
                compose.onRoot().captureToImage().asAndroidBitmap().compress(android.graphics.Bitmap.CompressFormat.PNG, 100, output)
            }
            compose.onNodeWithContentDescription("Close search").performClick()
            compose.onNodeWithText("Search fixture").assertIsDisplayed()
            compose.onNodeWithContentDescription("Back").assertIsDisplayed()
            compose.onNodeWithContentDescription("Search messages").assertDoesNotExist()
        }
    }
}
