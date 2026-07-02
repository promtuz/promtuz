package com.promtuz.chat.ui.theme

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.ui.platform.*

/**
 * TODO:
 *  Need additional app level theme apart from material theme for customizing app level components
 */
@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun PromtuzTheme(
    darkTheme: Boolean = isSystemInDarkTheme(),
    dynamicTheme: Boolean = false,
    content: @Composable () -> Unit
) {
    val context = LocalContext.current

    val colorScheme =
        if (darkTheme) {
            if (dynamicTheme) dynamicDarkColorScheme(context) else primaryColors
        } else dynamicLightColorScheme(context)


    CompositionLocalProvider(LocalTheme provides Theme(ThemeMode.fromBool(darkTheme))) {
        MaterialExpressiveTheme(
            colorScheme = colorScheme,
            typography = Typography,
            content = content,
        )
    }
}

data class Theme(val mode: ThemeMode)

enum class ThemeMode {
    Dark, Light;

    companion object {
        fun fromBool(isDark: Boolean): ThemeMode {
            return if (isDark) Dark else Light
        }
    }
}

val LocalTheme = compositionLocalOf<Theme> {
    error("No Theme provided.")
}