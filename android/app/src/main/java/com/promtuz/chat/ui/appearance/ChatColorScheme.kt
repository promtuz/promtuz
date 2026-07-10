package com.promtuz.chat.ui.appearance

import androidx.compose.material3.ColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.remember
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import dev.chrisbanes.haze.HazeStyle
import dev.chrisbanes.haze.HazeTint

/**
 * The chat's resolved color vocabulary — semantic roles the M3 [ColorScheme] has no
 * slot for. Chat renderers read this (via [LocalChatColors]) instead of reaching into
 * the scheme, so a preset can recolor the whole conversation without touching the app
 * shell. Resolved once at the theme root from the user's [ChatColors] tokens; `null`
 * tokens fall back to scheme roles, so the default preset tracks light/dark for free.
 */
@Immutable
data class ChatColorScheme(
    val outgoingBubble: Color,
    val onOutgoingBubble: Color,
    val incomingBubble: Color,
    val onIncomingBubble: Color,
    /** Send button, cursor, typing indicator — the chat's active color. */
    val accent: Color,
    /** Translucent top/bottom bar tint base (the haze). */
    val bar: Color,
    /** Frontier marker dashes + labels (alpha applied at use). */
    val marker: Color,
)

val LocalChatColors = staticCompositionLocalOf<ChatColorScheme> {
    error("No ChatColorScheme provided — PromtuzTheme mounts it.")
}

fun ChatColors.resolve(scheme: ColorScheme) = ChatColorScheme(
    outgoingBubble = outgoing.orRole(scheme.primaryContainer),
    onOutgoingBubble = outgoingText.orRole(scheme.onPrimaryContainer),
    incomingBubble = incoming.orRole(scheme.surfaceContainerHigh),
    onIncomingBubble = incomingText.orRole(scheme.onSurface),
    accent = accent.orRole(scheme.primary),
    bar = scheme.surface,
    marker = scheme.onSurfaceVariant,
)

private fun Long?.orRole(role: Color): Color = this?.let { Color(it) } ?: role

/** The one blur recipe both chat bars share. */
@Composable
fun chatBarHaze(): HazeStyle {
    val bar = LocalChatColors.current.bar
    return remember(bar) { HazeStyle(bar, HazeTint(bar.copy(alpha = 0.5f)), 30.dp, 0f) }
}
