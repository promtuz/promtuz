package com.promtuz.chat.ui.theme

import androidx.compose.material3.ColorScheme
import androidx.compose.ui.graphics.Color
import kotlinx.serialization.Serializable

/** Whole-app colour sets picked in Appearance. Graphite carries the logo cyan and is the default. */
@Serializable
enum class Palette(val label: String) {
    Graphite("Graphite"),
    Classic("Classic"),
    Ink("Ink"),
}

fun Palette.colors(dark: Boolean): PromtuzColors = when (this) {
    Palette.Graphite -> if (dark) DarkPromtuzColors else LightPromtuzColors
    Palette.Classic -> if (dark) ClassicDark else ClassicLight
    Palette.Ink -> if (dark) InkDark else InkLight
}

/** The Material roles follow the palette so screens still on stock components recolour with it. */
fun PromtuzColors.scheme(): ColorScheme = (if (isDark) DarkColors else LightColors).copy(
    primary = accentFill,
    surfaceTint = accentFill,
    primaryContainer = bubbleOut,
    onPrimaryContainer = onBubbleOut,
    background = page,
    surface = page,
    onBackground = text,
    onSurface = text,
    onSurfaceVariant = muted,
    surfaceVariant = cardHigh,
    outlineVariant = divider,
    error = danger,
    surfaceContainerLow = if (isDark) card else groupedPage,
    surfaceContainer = if (isDark) bubbleIn else cardHigh,
    surfaceContainerHigh = cardHigh,
)

private val ClassicDark = DarkPromtuzColors.copy(
    page = Color(0xFF12171D), groupedPage = Color(0xFF12171D), card = Color(0xFF1B2129),
    cardHigh = Color(0xFF242C35), divider = Color(0xFF28313B), text = Color(0xFFE7ECF1),
    muted = Color(0xFF8C98A5), thumbOff = Color(0xFF8C98A5), accent = Color(0xFF6BA3EC),
    accentFill = Color(0xFF5A91D8), switchOff = Color(0xFF38424D),
    bubbleOut = Color(0xFF2C4669), onBubbleOut = Color(0xFFE3ECF8), bubbleIn = Color(0xFF1F252D),
    wallpaper = Color(0xFF12171D),
)

private val ClassicLight = LightPromtuzColors.copy(
    groupedPage = Color(0xFFEFF2F5), cardHigh = Color(0xFFE8ECF0), divider = Color(0xFFE4E8ED),
    text = Color(0xFF10151A), muted = Color(0xFF68737F), accent = Color(0xFF2769C6),
    accentFill = Color(0xFF2F73D0), switchOff = Color(0xFFC7CED6), bubbleOut = Color(0xFFD4E6FF),
    onBubbleOut = Color(0xFF0C2C5A), wallpaper = Color(0xFFE5ECF5),
)

private val InkDark = DarkPromtuzColors.copy(
    page = Color(0xFF0C0E13), groupedPage = Color(0xFF0C0E13), card = Color(0xFF161922),
    cardHigh = Color(0xFF1F2330), divider = Color(0xFF222634), text = Color(0xFFEEF0F5),
    muted = Color(0xFF8A90A0), thumbOff = Color(0xFF8A90A0), accent = Color(0xFF5B95FF),
    accentFill = Color(0xFF2F6BFF), switchOff = Color(0xFF343A4A),
    bubbleOut = Color(0xFF2F6BFF), onBubbleOut = Color(0xFFFFFFFF), bubbleIn = Color(0xFF1A1E28),
    wallpaper = Color(0xFF0C0E13),
)

private val InkLight = LightPromtuzColors.copy(
    groupedPage = Color(0xFFF0F2F7), cardHigh = Color(0xFFE7EAF2), divider = Color(0xFFE3E6EE),
    text = Color(0xFF0F121A), muted = Color(0xFF646B7B), accent = Color(0xFF2456E0),
    accentFill = Color(0xFF2F6BFF), switchOff = Color(0xFFC5CAD6), bubbleOut = Color(0xFF2F6BFF),
    onBubbleOut = Color(0xFFFFFFFF), wallpaper = Color(0xFFE6E9F2),
)
