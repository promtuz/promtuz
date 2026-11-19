package com.promtuz.chat.ui.components

import androidx.compose.runtime.Composable
import androidx.compose.runtime.State
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.foundation.background
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.ui.draw.dropShadow
import androidx.compose.ui.graphics.shadow.Shadow
import androidx.compose.ui.unit.DpOffset
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.ui.Modifier
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.draw.drawWithCache
import androidx.compose.ui.graphics.ColorFilter
import androidx.compose.ui.graphics.ColorMatrix
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.PathOperation
import androidx.compose.ui.graphics.addOutline
import dev.chrisbanes.haze.blur.HazeBlurStyleScope
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.lerp
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.theme.Radii
import dev.chrisbanes.haze.HazeInput
import dev.chrisbanes.haze.HazeProgressive
import androidx.compose.ui.graphics.Brush
import androidx.compose.animation.core.EaseIn
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.blur.HazeBlurStyle
import dev.chrisbanes.haze.blur.HazeColorEffect
import dev.chrisbanes.haze.blur.hazeBlur

enum class FrostRole { Bar, ChatBar, Sheet, Floating }

/** Sources the whole activity content, so separate windows (dialogs, popups, sheets) can blur what lies under them. */
val LocalRootHaze = staticCompositionLocalOf<HazeState?> { null }

/** True inside a frosted host (sheet, dialog, menu, composer panel); cards and fills there turn translucent. */
val LocalOnGlass = staticCompositionLocalOf { false }

/** A card's fill: opaque on a page, translucent on glass. */
@Composable
fun cardFill(): Color = if (LocalOnGlass.current) Promtuz.colors.glassCard else Promtuz.colors.card

/** A raised fill inside a card or a selection: opaque on a page, translucent on glass. */
@Composable
fun raisedFill(): Color = if (LocalOnGlass.current) Promtuz.colors.glassFill else Promtuz.colors.cardHigh

val SheetShape = RoundedCornerShape(topStart = Radii.sheet, topEnd = Radii.sheet)

/**
 * Menus, pickers, dialogs and toasts. The default root state only works from another window; a surface
 * drawn inside the activity content passes its screen's own state.
 */
@Composable
fun Modifier.floatingSurface(shape: Shape, haze: HazeState? = LocalRootHaze.current): Modifier {
    val c = Promtuz.colors
    val shadow = remember(c.isDark) {
        Shadow(28.dp, Color.Black.copy(alpha = if (c.isDark) 0.4f else 0.14f), offset = DpOffset(0.dp, 8.dp))
    }
    val shadowed = dropShadow(shape, shadow)
    return if (haze != null) shadowed.frost(haze, FrostRole.Floating, shape) else shadowed.clip(shape).background(c.card)
}

@Composable
fun Modifier.sheetSurface(haze: HazeState? = LocalRootHaze.current): Modifier =
    if (haze != null) frost(haze, FrostRole.Sheet, SheetShape) else clip(SheetShape).background(Promtuz.colors.card)

@Composable
private fun rememberFrostStyle(role: FrostRole, scrolled: State<() -> Float>, page: Color): HazeBlurStyle {
    val c = Promtuz.colors
    val chatBar = LocalChatColors.current.bar
    return remember(role, c, chatBar, page) {
        HazeBlurStyle {
            noiseFactor(0f)
            when (role) {
                // At rest the bar is identical to the page; it turns to card glass as content slides under.
                FrostRole.Bar -> {
                    val tint = lerp(page, c.card, scrolled.value().coerceIn(0f, 1f))
                    frost(page, tint.copy(alpha = if (c.isDark) 0.76f else 0.74f), 36f)
                }
                // Sheets, dialogs and menus share the composer's glass so every floating layer reads alike.
                FrostRole.ChatBar, FrostRole.Sheet, FrostRole.Floating -> frost(chatBar, chatBar.copy(alpha = 0.76f), 38f)
            }
        }
    }
}

private val Saturate = HazeColorEffect.colorFilter(ColorFilter.colorMatrix(ColorMatrix().apply { setToSaturation(3f) }))

private fun HazeBlurStyleScope.frost(base: Color, tint: Color, radius: Float) {
    backgroundColor(base)
    blurRadius(radius.dp)
    colorEffects(listOf(Saturate, HazeColorEffect.tint(tint)))
    fallbackColorEffect(HazeColorEffect.tint(tint.copy(alpha = 1f)))
}

/**
 * The one way to frost a surface. Clip comes first because the blur itself only clips to a rectangle.
 * Shaped surfaces get the rim, which keeps the edge visible when the backdrop matches the fill.
 */
@Composable
fun Modifier.frost(
    state: HazeState,
    role: FrostRole,
    shape: Shape? = null,
    scrolled: () -> Float = { 1f },
    page: Color = Promtuz.colors.page,
): Modifier {
    val c = Promtuz.colors
    val style = rememberFrostStyle(role, rememberUpdatedState(scrolled), page)
    if (shape != null) {
        val halo = if (c.isDark) this else dropShadow(shape, RimHalo)
        // A sheet rises from the bottom edge, so only its top is lit.
        return halo.clip(shape).hazeBlur(HazeInput.Sources(state), style).rim(shape, c.isDark, bottom = role != FrostRole.Sheet)
    }
    val frosted = hazeBlur(HazeInput.Sources(state), style)
    if (role != FrostRole.Bar) return frosted
    val edge = c.glassEdge
    return frosted.drawWithContent {
        drawContent()
        val a = scrolled().coerceIn(0f, 1f)
        if (a > 0f) drawLine(edge.copy(alpha = edge.alpha * a), Offset(0f, size.height - 0.5f),
            Offset(size.width, size.height - 0.5f), 1.dp.toPx())
    }
}

private val RimHalo = Shadow(1.dp, Color.Black.copy(alpha = 0.125f), offset = DpOffset(0.dp, 0.33.dp))

/** Two inner crescents lit from above: the shape minus itself shifted down, and minus itself shifted up. */
private fun Modifier.rim(shape: Shape, dark: Boolean, bottom: Boolean = true): Modifier = drawWithCache {
    val body = Path().apply { addOutline(shape.createOutline(size, layoutDirection, this@drawWithCache)) }
    fun crescent(dy: Float) = Path().apply {
        op(body, Path().apply { addPath(body, Offset(0f, dy)) }, PathOperation.Difference)
    }
    val top = crescent(1.dp.toPx())
    val lower = if (bottom) crescent(-0.67.dp.toPx()) else null
    val topColor = Color.White.copy(alpha = if (dark) 0.157f else 1f)
    val bottomColor = Color.White.copy(alpha = if (dark) 0.078f else 1f)
    // Over the content, so edge-to-edge children (a photo grid, a highlighted row) never hide the edge.
    onDrawWithContent {
        drawContent()
        drawPath(top, topColor)
        if (lower != null) drawPath(lower, bottomColor)
    }
}

/**
 * A top bar's backdrop with no edge: the composer's glass, its blur and tint thinning to nothing at the bar's
 * bottom. Goes behind the bar's content as a sibling.
 */
@Composable
fun FrostFade(haze: HazeState, modifier: Modifier = Modifier) {
    val bar = LocalChatColors.current.bar
    val style = remember(bar) {
        HazeBlurStyle {
            noiseFactor(0f)
            backgroundColor(bar)
            blurRadius(32.dp)
            // Held strong through the subtitle line, then thinned to nothing at the bar's foot.
            progressive(HazeProgressive.verticalGradient(easing = EaseIn, startIntensity = 1f, endIntensity = 0f))
            // No saturation boost: at rest over a plain page the bar must match it exactly.
            val tint = HazeColorEffect.tint(Brush.verticalGradient(
                0f to bar.copy(alpha = 0.78f), 0.7f to bar.copy(alpha = 0.62f), 1f to bar.copy(alpha = 0f)))
            colorEffects(listOf(tint))
            fallbackColorEffect(tint)
        }
    }
    androidx.compose.foundation.layout.Box(modifier.hazeBlur(HazeInput.Sources(haze), style))
}
