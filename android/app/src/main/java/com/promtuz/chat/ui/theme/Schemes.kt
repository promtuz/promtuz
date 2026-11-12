package com.promtuz.chat.ui.theme

import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.ui.graphics.Color

// Graphite surfaces with the logo cyan as accent; mirrors the v2dark/v2light Figma variables.
val DarkColors = darkColorScheme(
    primary = Color(0xFF1C93F5),
    onPrimary = Color(0xFFFFFFFF),
    primaryContainer = Color(0xFF11384F),
    onPrimaryContainer = Color(0xFFE3F6FF),
    inversePrimary = Color(0xFF0079C7),

    secondary = Color(0xFF7FB8D4),
    onSecondary = Color(0xFF07202C),
    secondaryContainer = Color(0xFF1D3340),
    onSecondaryContainer = Color(0xFFCFEAF7),

    tertiary = Color(0xFFB77A8D),
    onTertiary = Color(0xFFF4EAEC),
    tertiaryContainer = Color(0xFF422731),
    onTertiaryContainer = Color(0xFFDAB7C0),

    error = Color(0xFFEF6E6A),
    onError = Color(0xFFFFFFFF),
    errorContainer = Color(0xFF601410),
    onErrorContainer = Color(0xFFFFB4AB),

    background = Color(0xFF101113),
    onBackground = Color(0xFFECEDEF),
    surface = Color(0xFF101113),
    onSurface = Color(0xFFECEDEF),
    surfaceVariant = Color(0xFF232529),
    onSurfaceVariant = Color(0xFF8E9196),
    surfaceTint = Color(0xFF1C93F5),
    inverseSurface = Color(0xFFE6E7E9),
    inverseOnSurface = Color(0xFF121315),

    outline = Color(0xFF55585D),
    outlineVariant = Color(0xFF26282C),
    scrim = Color(0xFF000000),

    surfaceBright = Color(0xFF2A2C30),
    surfaceDim = Color(0xFF0C0D0E),
    surfaceContainerLowest = Color(0xFF0B0C0D),
    surfaceContainerLow = Color(0xFF1A1C1F),
    surfaceContainer = Color(0xFF1E2023),
    surfaceContainerHigh = Color(0xFF232529),
    surfaceContainerHighest = Color(0xFF2A2C30),
)

val LightColors = lightColorScheme(
    primary = Color(0xFF0079C7),
    onPrimary = Color(0xFFFFFFFF),
    primaryContainer = Color(0xFFD6F1FF),
    onPrimaryContainer = Color(0xFF06324A),
    inversePrimary = Color(0xFF3CC8FF),

    secondary = Color(0xFF3B6F88),
    onSecondary = Color(0xFFFFFFFF),
    secondaryContainer = Color(0xFFDDF2FC),
    onSecondaryContainer = Color(0xFF0F3346),

    tertiary = Color(0xFF8E5266),
    onTertiary = Color(0xFFFFFFFF),
    tertiaryContainer = Color(0xFFF6DAE2),
    onTertiaryContainer = Color(0xFF3A222B),

    error = Color(0xFFD63B36),
    onError = Color(0xFFFFFFFF),
    errorContainer = Color(0xFFFFDAD6),
    onErrorContainer = Color(0xFF410002),

    background = Color(0xFFFFFFFF),
    onBackground = Color(0xFF111214),
    surface = Color(0xFFFFFFFF),
    onSurface = Color(0xFF111214),
    surfaceVariant = Color(0xFFEAEBEE),
    onSurfaceVariant = Color(0xFF6B6F76),
    surfaceTint = Color(0xFF0079C7),
    inverseSurface = Color(0xFF2A2C30),
    inverseOnSurface = Color(0xFFF1F2F4),

    outline = Color(0xFF7C8087),
    outlineVariant = Color(0xFFE5E6E9),
    scrim = Color(0xFF000000),

    surfaceBright = Color(0xFFFFFFFF),
    surfaceDim = Color(0xFFD9DADE),
    surfaceContainerLowest = Color(0xFFFFFFFF),
    surfaceContainerLow = Color(0xFFF1F2F4),
    surfaceContainer = Color(0xFFEAEBEE),
    surfaceContainerHigh = Color(0xFFE4E5E8),
    surfaceContainerHighest = Color(0xFFDDDEE2),
)
