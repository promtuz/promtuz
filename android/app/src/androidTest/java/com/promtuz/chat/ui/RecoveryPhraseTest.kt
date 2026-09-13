package com.promtuz.chat.ui

import android.graphics.Bitmap
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.captureToImage
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.unit.Density
import androidx.test.platform.app.InstrumentationRegistry
import com.promtuz.chat.security.RecoveryPhraseState
import com.promtuz.chat.ui.screens.IdentityKeysScreen
import com.promtuz.chat.ui.screens.RecoveryPhraseContent
import com.promtuz.chat.ui.theme.PromtuzTheme
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import java.io.File

class RecoveryPhraseTest {
    @get:Rule val compose = createComposeRule()

    @Test fun identityPageHasSeparatePublicAndPrivateActions() {
        var shares = 0
        var recoveries = 0
        compose.setContent { PromtuzTheme(darkTheme = true) {
            IdentityKeysScreen({ shares++ }, { recoveries++ })
        } }
        saveScreenshot("identity-keys-dark.png")
        compose.onNodeWithText("Identity & Keys").assertIsDisplayed()
        compose.onNodeWithText("My QR code").performScrollTo().performClick()
        compose.onNodeWithText("Recovery phrase").performScrollTo().performClick()
        compose.runOnIdle { assertEquals(1, shares); assertEquals(1, recoveries) }
    }

    @Test fun noScreenLockOffersSetupInsteadOfReveal() {
        var settings = 0
        compose.setContent { PromtuzTheme(darkTheme = false) {
            RecoveryPhraseContent(RecoveryPhraseState.Locked(), false, false,
                {}, {}, {}, { settings++ })
        } }
        saveScreenshot("recovery-no-lock-light.png")
        compose.onNodeWithText("Show recovery phrase").assertDoesNotExist()
        compose.onNodeWithText("Open security settings").performScrollTo().performClick()
        compose.runOnIdle { assertEquals(1, settings) }
    }

    @Test fun revealIsDeliberateAndBusyStateCannotSubmitAgain() {
        var state: RecoveryPhraseState by mutableStateOf(RecoveryPhraseState.Locked())
        var reveals = 0
        compose.setContent { PromtuzTheme(darkTheme = true) {
            RecoveryPhraseContent(state, true, false,
                { reveals++; state = RecoveryPhraseState.Authenticating }, {},
                { state = RecoveryPhraseState.Locked() }, {})
        } }
        saveScreenshot("recovery-locked-dark.png")
        compose.runOnIdle { assertEquals(0, reveals) }
        compose.onNodeWithText("Show recovery phrase").performScrollTo().performClick()
        compose.onNodeWithText("Show recovery phrase").assertDoesNotExist()
        compose.onNodeWithText("Verifying…").assertIsDisplayed()
        compose.onNodeWithText("Cancel").performScrollTo().performClick()
        compose.onNodeWithText("Show recovery phrase").assertIsDisplayed()
        compose.runOnIdle { assertEquals(1, reveals) }
    }

    @Test fun deviceCredentialActionWorksWithoutSelectingBiometrics() {
        var credentials = 0
        compose.setContent { PromtuzTheme {
            RecoveryPhraseContent(RecoveryPhraseState.Locked(), true, false,
                {}, { credentials++ }, {}, {})
        } }
        if (android.os.Build.VERSION.SDK_INT >= 30) {
            compose.onNodeWithText("Use screen lock").performScrollTo().performClick()
            compose.runOnIdle { assertEquals(1, credentials) }
        }
    }

    @Test fun syntheticWordsAreNumberedAndCanBeHidden() {
        var state: RecoveryPhraseState by mutableStateOf(RecoveryPhraseState.Revealed(List(24) { "example" }))
        compose.setContent { PromtuzTheme(darkTheme = false) {
            RecoveryPhraseContent(state, true, false, {}, {}, { state = RecoveryPhraseState.Locked() }, {})
        } }
        compose.onNodeWithContentDescription("Word 1: example").performScrollTo().assertIsDisplayed()
        saveScreenshot("recovery-words-light.png")
        compose.onNodeWithContentDescription("Word 24: example").performScrollTo().assertIsDisplayed()
        compose.onNodeWithTag("recovery-hide-bottom").performScrollTo()
        saveScreenshot("recovery-bottom-light.png")
        compose.onNodeWithTag("recovery-hide-bottom").assertIsDisplayed().performClick()
        compose.onNodeWithContentDescription("Word 1: example").assertDoesNotExist()
    }

    @Test fun largeTextKeepsLastWordAndHideActionReachable() {
        compose.setContent {
            val density = LocalDensity.current
            CompositionLocalProvider(LocalDensity provides Density(density.density, 2f)) {
                PromtuzTheme(darkTheme = true) {
                    RecoveryPhraseContent(RecoveryPhraseState.Revealed(List(24) { "example" }), true, false, {}, {}, {}, {})
                }
            }
        }
        compose.onNodeWithContentDescription("Word 24: example").performScrollTo().assertIsDisplayed()
        compose.onNodeWithTag("recovery-hide-bottom").performScrollTo()
        saveScreenshot("recovery-large-text-dark.png")
        compose.onNodeWithTag("recovery-hide-bottom").assertIsDisplayed()
        compose.onNodeWithTag("recovery-hide-top").assertIsDisplayed()
    }

    private fun saveScreenshot(name: String) {
        // Content-only tests use synthetic words. Never capture the real secure screen.
        val screenshot = compose.onRoot().captureToImage().asAndroidBitmap()
        val directory = InstrumentationRegistry.getInstrumentation().targetContext.cacheDir
        File(directory, name).outputStream().use { screenshot.compress(Bitmap.CompressFormat.PNG, 100, it) }
    }
}
