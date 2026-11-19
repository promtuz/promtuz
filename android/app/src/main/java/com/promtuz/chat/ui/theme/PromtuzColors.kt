package com.promtuz.chat.ui.theme

import androidx.compose.runtime.Composable
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.ReadOnlyComposable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color

/** Roles the Material scheme has no slot for. Light grouped pages are grey with white cards; dark ones lift the card instead. */
@Immutable
data class PromtuzColors(
    val page: Color,
    val groupedPage: Color,
    val card: Color,
    val cardHigh: Color,
    val divider: Color,
    val text: Color,
    val muted: Color,
    val accent: Color,
    val accentFill: Color,
    val switchOff: Color,
    val thumbOff: Color,
    val danger: Color,
    val success: Color,
    val warn: Color,
    val warnBg: Color,
    val glassEdge: Color,
    val bubbleOut: Color,
    val onBubbleOut: Color,
    val bubbleIn: Color,
    val wallpaper: Color,
    val isDark: Boolean,
) {
    /** A selection or hover on glass: translucent, so the frost still shows through. */
    val glassFill: Color get() = text.copy(alpha = if (isDark) 0.1f else 0.07f)

    /** A card or field resting on glass, light enough that the frost reads through it. */
    val glassCard: Color get() = text.copy(alpha = if (isDark) 0.06f else 0.04f)
}

val DarkPromtuzColors = PromtuzColors(
    page = Color(0xFF101113),
    groupedPage = Color(0xFF101113),
    card = Color(0xFF1A1C1F),
    cardHigh = Color(0xFF232529),
    divider = Color(0xFF26282C),
    text = Color(0xFFECEDEF),
    muted = Color(0xFF8E9196),
    accent = Color(0xFF3CC8FF),
    accentFill = Color(0xFF1C93F5),
    switchOff = Color(0xFF383A3F),
    thumbOff = Color(0xFF8E9196),
    danger = Color(0xFFEF6E6A),
    success = Color(0xFF58C08A),
    warn = Color(0xFFF3A35C),
    warnBg = Color(0xFF3A2A1F),
    glassEdge = Color(0x14FFFFFF),
    bubbleOut = Color(0xFF11384F),
    onBubbleOut = Color(0xFFE3F6FF),
    bubbleIn = Color(0xFF1E2023),
    wallpaper = Color(0xFF101113),
    isDark = true,
)

val LightPromtuzColors = PromtuzColors(
    page = Color(0xFFFFFFFF),
    groupedPage = Color(0xFFF1F2F4),
    card = Color(0xFFFFFFFF),
    cardHigh = Color(0xFFEAEBEE),
    divider = Color(0xFFE5E6E9),
    text = Color(0xFF111214),
    muted = Color(0xFF6B6F76),
    accent = Color(0xFF0079C7),
    accentFill = Color(0xFF0A86E0),
    switchOff = Color(0xFFC9CCD1),
    thumbOff = Color(0xFFFFFFFF),
    danger = Color(0xFFD63B36),
    success = Color(0xFF249A5B),
    warn = Color(0xFFB9600F),
    warnBg = Color(0xFFFFF1E5),
    glassEdge = Color(0x0F111214),
    bubbleOut = Color(0xFFD6F1FF),
    onBubbleOut = Color(0xFF06324A),
    bubbleIn = Color(0xFFFFFFFF),
    wallpaper = Color(0xFFE8EBEE),
    isDark = false,
)

val LocalPromtuzColors = staticCompositionLocalOf { DarkPromtuzColors }

object Promtuz {
    val colors: PromtuzColors
        @Composable @ReadOnlyComposable get() = LocalPromtuzColors.current
}
