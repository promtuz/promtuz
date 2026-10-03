package com.promtuz.chat.ui.appearance

import kotlinx.serialization.Serializable

@Serializable
data class ChatAppearance(
    val bubble: BubbleStyle = BubbleStyle(),
    val layout: LayoutStyle = LayoutStyle(),
    val colors: ChatColors = ChatColors(),
    val wallpaper: Wallpaper = Wallpaper.Default,
    val type: TypeStyle = TypeStyle(),
    val interaction: InteractionStyle = InteractionStyle(),
    val themeMode: ThemeMode = ThemeMode.System,
) {
    companion object {
        val Default = ChatAppearance()
    }
}

/** Bubble geometry (dp). The near-corner is the collapsed radius on merged edges. */
@Serializable
data class BubbleStyle(
    val cornerRadius: Float = 18f,
    val nearCornerRadius: Float = 6f,
    val tail: Boolean = true,
    val tailSize: Float = 8f,
)

@Serializable
data class LayoutStyle(
    /** Same-author messages within this window merge into one group. */
    val mergeWindowSecs: Int = 300,
    /** dp between merged messages. */
    val messageGap: Float = 2f,
    /** dp between groups. */
    val groupGap: Float = 8f,
    val maxWidthFraction: Float = 0.75f,
)

/** ARGB; null uses the scheme's designed default (see [resolve]). */
@Serializable
data class ChatColors(
    val outgoing: Long? = null,
    val incoming: Long? = null,
    val outgoingText: Long? = null,
    val incomingText: Long? = null,
    val accent: Long? = null,
)

@Serializable
data class TypeStyle(
    val fontScale: Float = 1f,
)

@Serializable
data class InteractionStyle(
    val doubleTapAction: DoubleTapAction = DoubleTapAction.React,
    val doubleTapEmoji: String = "❤️",
)

@Serializable
enum class DoubleTapAction { None, React, Reply, Edit }

/** [Pattern] is the app's built-in chat pattern. */
@Serializable
sealed interface Wallpaper {
    @Serializable
    data class Solid(val argb: Long) : Wallpaper

    @Serializable
    data class Pattern(val tintArgb: Long? = null, val alpha: Float = 0.1f) : Wallpaper

    companion object {
        val Default: Wallpaper = Pattern()
    }
}

@Serializable
enum class ThemeMode { System, Light, Dark }
