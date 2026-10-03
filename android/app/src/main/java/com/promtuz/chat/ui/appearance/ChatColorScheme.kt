package com.promtuz.chat.ui.appearance

import androidx.compose.material3.ColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.remember
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.luminance
import androidx.compose.ui.unit.dp
import dev.chrisbanes.haze.HazeStyle
import dev.chrisbanes.haze.HazeTint

/** Chat renderers read colors from here, never the scheme, so a preset recolors only the conversation. */
@Immutable
data class ChatColorScheme(
    val outgoingBubble: Color,
    val onOutgoingBubble: Color,
    val incomingBubble: Color,
    val onIncomingBubble: Color,
    val accent: Color,
    /** Tint base for the translucent bars' haze. */
    val bar: Color,
    /** System-event labels (alpha applied at use). */
    val marker: Color,
    /** Kept off the accent so a sender's name never looks tappable. */
    val senderPalette: List<Color>,
)

val LocalChatColors = staticCompositionLocalOf<ChatColorScheme> {
    error("No ChatColorScheme provided — PromtuzTheme mounts it.")
}

fun ChatColors.resolve(scheme: ColorScheme) = ChatColorScheme(
    outgoingBubble = outgoing.orRole(scheme.primaryContainer),
    onOutgoingBubble = outgoingText.orRole(outgoing?.let(::bestOn) ?: scheme.onPrimaryContainer),
    incomingBubble = incoming.orRole(scheme.surfaceContainerHigh),
    onIncomingBubble = incomingText.orRole(incoming?.let(::bestOn) ?: scheme.onSurface),
    accent = accent.orRole(scheme.primary),
    bar = scheme.surface,
    marker = scheme.onSurfaceVariant,
    senderPalette = SENDER_PALETTE,
)

/** Fixed hues: a ramp off one seed color can't keep them distinguishable. */
private val SENDER_PALETTE = listOf(
    Color(0xFF4E8FD9), // blue
    Color(0xFFCF6E5B), // terracotta
    Color(0xFF56A177), // green
    Color(0xFFB07CC6), // violet
    Color(0xFFD09A3C), // amber
    Color(0xFF4FA3A8), // teal
    Color(0xFFD4708F), // rose
)

private fun Long?.orRole(role: Color): Color = this?.let { Color(it) } ?: role

private fun bestOn(argb: Long): Color =
    if (Color(argb).luminance() > 0.4f) Color(0xE6000000) else Color.White

@Composable
fun chatBarHaze(): HazeStyle {
    val bar = LocalChatColors.current.bar
    return remember(bar) { HazeStyle(bar, HazeTint(bar.copy(alpha = 0.5f)), 30.dp, 0f) }
}
