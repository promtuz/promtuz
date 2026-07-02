package com.promtuz.chat.ui.theme

import androidx.compose.material3.*
import androidx.compose.ui.graphics.*

/**
 * TODO:
 *  This shit is bad and inconsistent, fix it
 *
 *  Primary Theme Hue = 214
 */
val primaryColors = ColorScheme(
    background = Color(0xFF111418),

    error = Color(0xFFF2B8B5),
    errorContainer = Color(0xFF601410),

    inversePrimary = Color(0xFF9CBDE8),
    inverseSurface = Color(0xFFE4E5EB),

    outline = Color(0xFF5E6069),
    outlineVariant = Color(0xFF3B3D45),

    scrim = Color(0xFF000000),

    primary = Color(0xFF5A91D8),
    primaryContainer = Color(0xFF2C4669),
    primaryFixed = Color(0xFFA2BBDC),
    primaryFixedDim = Color(0xFF7DA0CF),

    secondary = Color(0xFF5E91A1),
    secondaryContainer = Color(0xFF23373F),
    secondaryFixed = Color(0xFF30505A),
    secondaryFixedDim = Color(0xFF1E343C),

    tertiary = Color(0xFFB77A8D),
    tertiaryContainer = Color(0xFF422731),
    tertiaryFixed = Color(0xFF593743),
    tertiaryFixedDim = Color(0xFF3A242E),

    // Surface Colors
    surface = Color(0xFF12151A),
    surfaceBright = Color(0xFF2F323A),
    surfaceContainer = Color(0xFF1C1E24),
    surfaceContainerHigh = Color(0xFF24262C),
    surfaceContainerHighest = Color(0xFF2D2F35),
    surfaceContainerLow = Color(0xFF17191E),
    surfaceContainerLowest = Color(0xFF0A0C10),
    surfaceDim = Color(0xFF101214),
    surfaceTint = Color(0xFF659ADF),
    surfaceVariant = Color(0xFF3F424C),

    // Typographic Colors
    onBackground = Color(0xFFDFE5EC),

    onError = Color(0xFFF9DEDC),
    onErrorContainer = Color(0xFFFFB4AB),

    onPrimary = Color(0xFF000714),
    onPrimaryContainer = Color(0xFFBFD4F2),
    onPrimaryFixed = Color(0xFFD9E5FF),
    onPrimaryFixedVariant = Color(0xFFAEC7EE),

    onSecondary = Color(0xFFE8F2F4),
    onSecondaryContainer = Color(0xFFB9D6DE),
    onSecondaryFixed = Color(0xFFD3E8EE),
    onSecondaryFixedVariant = Color(0xFFA3C9D4),

    onTertiary = Color(0xFFF4EAEC),
    onTertiaryContainer = Color(0xFFDAB7C0),
    onTertiaryFixed = Color(0xFFF2DCE2),
    onTertiaryFixedVariant = Color(0xFFD0A6B0),

    onSurface = Color(0xFFd0d1d7),
    inverseOnSurface = Color(0xFF121418),
    onSurfaceVariant = Color(0xFFB9BAC3),
)