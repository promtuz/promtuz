package com.promtuz.chat.ui.theme

import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.ui.graphics.Color

/*
 * The app's designed identity — hue 214 blue over cool neutrals, hand-authored for
 * both modes (no wallpaper seeding; dynamic color is an explicit opt-in in Theme.kt).
 * Chat-specific roles the scheme has no slot for live in ui/appearance/ChatColorScheme.
 */

val DarkColors = darkColorScheme(
    primary = Color(0xFF5A91D8),
    onPrimary = Color(0xFF000714),
    primaryContainer = Color(0xFF2C4669),
    onPrimaryContainer = Color(0xFFBFD4F2),
    inversePrimary = Color(0xFF9CBDE8),

    secondary = Color(0xFF5E91A1),
    onSecondary = Color(0xFFE8F2F4),
    secondaryContainer = Color(0xFF23373F),
    onSecondaryContainer = Color(0xFFB9D6DE),

    tertiary = Color(0xFFB77A8D),
    onTertiary = Color(0xFFF4EAEC),
    tertiaryContainer = Color(0xFF422731),
    onTertiaryContainer = Color(0xFFDAB7C0),

    error = Color(0xFFF2B8B5),
    onError = Color(0xFFF9DEDC),
    errorContainer = Color(0xFF601410),
    onErrorContainer = Color(0xFFFFB4AB),

    background = Color(0xFF111418),
    onBackground = Color(0xFFDFE5EC),
    surface = Color(0xFF12151A),
    onSurface = Color(0xFFD0D1D7),
    surfaceVariant = Color(0xFF3F424C),
    onSurfaceVariant = Color(0xFFB9BAC3),
    surfaceTint = Color(0xFF659ADF),
    inverseSurface = Color(0xFFE4E5EB),
    inverseOnSurface = Color(0xFF121418),

    outline = Color(0xFF5E6069),
    outlineVariant = Color(0xFF3B3D45),
    scrim = Color(0xFF000000),

    surfaceBright = Color(0xFF2F323A),
    surfaceDim = Color(0xFF101214),
    surfaceContainerLowest = Color(0xFF0A0C10),
    surfaceContainerLow = Color(0xFF17191E),
    surfaceContainer = Color(0xFF1C1E24),
    surfaceContainerHigh = Color(0xFF24262C),
    surfaceContainerHighest = Color(0xFF2D2F35),
)

val LightColors = lightColorScheme(
    primary = Color(0xFF3A6EA8),
    onPrimary = Color(0xFFFFFFFF),
    primaryContainer = Color(0xFFD4E3F8),
    onPrimaryContainer = Color(0xFF102D4E),
    inversePrimary = Color(0xFF9CBDE8),

    secondary = Color(0xFF3E6674),
    onSecondary = Color(0xFFFFFFFF),
    secondaryContainer = Color(0xFFCFE6EC),
    onSecondaryContainer = Color(0xFF17343C),

    tertiary = Color(0xFF8E5266),
    onTertiary = Color(0xFFFFFFFF),
    tertiaryContainer = Color(0xFFF6DAE2),
    onTertiaryContainer = Color(0xFF3A222B),

    error = Color(0xFFBA1A1A),
    onError = Color(0xFFFFFFFF),
    errorContainer = Color(0xFFFFDAD6),
    onErrorContainer = Color(0xFF410002),

    background = Color(0xFFF6F8FB),
    onBackground = Color(0xFF181C22),
    surface = Color(0xFFF6F8FB),
    onSurface = Color(0xFF181C22),
    surfaceVariant = Color(0xFFDFE3EB),
    onSurfaceVariant = Color(0xFF43474F),
    surfaceTint = Color(0xFF3A6EA8),
    inverseSurface = Color(0xFF2D3036),
    inverseOnSurface = Color(0xFFEFF1F6),

    outline = Color(0xFF737781),
    outlineVariant = Color(0xFFC3C7D0),
    scrim = Color(0xFF000000),

    surfaceBright = Color(0xFFF6F8FB),
    surfaceDim = Color(0xFFD6DAE2),
    surfaceContainerLowest = Color(0xFFFFFFFF),
    surfaceContainerLow = Color(0xFFF0F3F8),
    surfaceContainer = Color(0xFFEAEDF3),
    surfaceContainerHigh = Color(0xFFE4E8EE),
    surfaceContainerHighest = Color(0xFFDEE2E9),
)
