package com.promtuz.chat.ui.theme

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.LocalIndication
import androidx.compose.material3.LocalRippleConfiguration
import androidx.compose.material3.RippleConfiguration
import androidx.compose.material.ripple.RippleAlpha
import androidx.compose.material3.MaterialExpressiveTheme
import com.promtuz.chat.ui.components.PressHighlight
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
    val palette = remember(appearance.palette, dark) { appearance.palette.colors(dark) }
    val colorScheme = remember(palette) { palette.scheme() }
    val view = androidx.compose.ui.platform.LocalView.current
    if (!view.isInEditMode) androidx.compose.runtime.SideEffect {
        var context = view.context
        while (context is android.content.ContextWrapper && context !is android.app.Activity) context = context.baseContext
        (context as? android.app.Activity)?.window?.let {
            androidx.core.view.WindowCompat.getInsetsController(it, view).apply {
                isAppearanceLightStatusBars = !dark
                isAppearanceLightNavigationBars = !dark
            }
        }
    }

    CompositionLocalProvider(
        LocalChatAppearance provides appearance,
        LocalPromtuzColors provides palette,
        LocalChatColors provides remember(appearance.colors, colorScheme, palette) {
            appearance.colors.resolve(colorScheme, palette)
        },
    ) {
        MaterialExpressiveTheme(
            colorScheme = colorScheme,
            typography = Typography,
        ) {
            // The app's ripple for everything it builds; stock Material controls keep theirs, matched in tint.
            CompositionLocalProvider(
                LocalIndication provides PressHighlight,
                LocalRippleConfiguration provides remember(palette) {
                    RippleConfiguration(palette.text, RippleAlpha(0.1f, 0.1f, 0.06f, 0.1f))
                },
                content = content,
            )
        }
    }
}
