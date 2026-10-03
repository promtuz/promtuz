package com.promtuz.chat.ui.theme

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialExpressiveTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.remember
import com.promtuz.chat.ui.appearance.ChatAppearance
import com.promtuz.chat.ui.appearance.LocalChatAppearance
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.appearance.ThemeMode
import com.promtuz.chat.ui.appearance.resolve

/** [appearance]'s themeMode overrides [darkTheme] unless it is System. */
@Composable
fun PromtuzTheme(
    darkTheme: Boolean = isSystemInDarkTheme(),
    appearance: ChatAppearance = ChatAppearance.Default,
    content: @Composable () -> Unit
) {
    val dark = when (appearance.themeMode) {
        ThemeMode.System -> darkTheme
        ThemeMode.Light -> false
        ThemeMode.Dark -> true
    }
    val colorScheme = if (dark) DarkColors else LightColors

    CompositionLocalProvider(
        LocalChatAppearance provides appearance,
        LocalChatColors provides remember(appearance.colors, colorScheme) {
            appearance.colors.resolve(colorScheme)
        },
    ) {
        MaterialExpressiveTheme(
            colorScheme = colorScheme,
            typography = Typography,
            content = content,
        )
    }
}
