package com.promtuz.chat.ui.appearance

import kotlinx.serialization.Serializable

/**
 * The chat's user-tweakable look — one serializable bundle (a "preset"). Nested
 * sub-styles so each settings section maps to one group and each renderer takes
 * only its slice. Values are platform-neutral primitives (Float dp, Long ARGB;
 * `null` color = derive from the M3 role) — converted to Compose types at the
 * `LocalChatAppearance` boundary, and shareable as a preset across clients.
 *
 * [Default] is our shipping baseline (Telegram-like), the seed the user tweaks.
 * Only one preset ships today; the bundle shape is the gate for more.
 */
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
        /** Shipping default preset (Telegram-baseline). */
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

/** List layout + grouping. */
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

/**
 * User color tokens (ARGB). `null` = the designed default from the scheme role — see
 * [resolve], which turns these into the full [ChatColorScheme] vocabulary.
 */
@Serializable
data class ChatColors(
    val outgoing: Long? = null,
    val incoming: Long? = null,
    val outgoingText: Long? = null,
    val incomingText: Long? = null,
    val accent: Long? = null,
)

/** Typography scaling. */
@Serializable
data class TypeStyle(
    val fontScale: Float = 1f,
)

/** Gesture behavior. */
@Serializable
data class InteractionStyle(
    val doubleTapAction: DoubleTapAction = DoubleTapAction.React,
    val doubleTapEmoji: String = "❤️",
)

@Serializable
enum class DoubleTapAction { None, React, Reply, Edit }

/** Chat background. Platform-neutral: [Pattern] = "the app's built-in chat pattern". */
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

/** App/chat theme mode. */
@Serializable
enum class ThemeMode { System, Light, Dark }
