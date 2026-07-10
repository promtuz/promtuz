package com.promtuz.chat.ui.theme

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.ExperimentalMaterial3ExpressiveApi
import androidx.compose.material3.MaterialExpressiveTheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalContext
import com.promtuz.chat.ui.appearance.ChatAppearance
import com.promtuz.chat.ui.appearance.LocalChatAppearance
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.appearance.resolve

/**
 * Designed identity by default ([DarkColors]/[LightColors]); [dynamicTheme] opts into
 * wallpaper-seeded Material You. Also mounts the chat appearance + its resolved
 * [LocalChatColors] — a persisted appearance store later feeds [appearance] here.
 */
@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun PromtuzTheme(
    darkTheme: Boolean = isSystemInDarkTheme(),
    dynamicTheme: Boolean = false,
    appearance: ChatAppearance = ChatAppearance.Default,
    content: @Composable () -> Unit
) {
    val context = LocalContext.current

    val colorScheme = when {
        dynamicTheme && darkTheme -> dynamicDarkColorScheme(context)
        dynamicTheme -> dynamicLightColorScheme(context)
        darkTheme -> DarkColors
        else -> LightColors
    }

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
