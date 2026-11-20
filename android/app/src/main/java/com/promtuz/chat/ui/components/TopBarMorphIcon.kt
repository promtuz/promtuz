package com.promtuz.chat.ui.components

import androidx.annotation.DrawableRes
import androidx.compose.foundation.layout.size
import androidx.compose.material3.LocalContentColor
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

object TopBarIconDefaults {
    val Size = 22.dp
    val StrokeWidth = 2.25.dp
}

/** The surrounding control owns its touch target, enabled state, and navigation. */
@Composable
fun TopBarMorphIcon(
    glyph: MorphGlyph,
    description: String?,
    modifier: Modifier = Modifier,
    tint: Color = LocalContentColor.current,
    strokeWidth: Dp = TopBarIconDefaults.StrokeWidth,
) = TopBarMorphIcon(rememberMorphIconState(glyph), description, modifier, tint, strokeWidth)

/** Use a hoisted [state] when the top bar switches between layouts. */
@Composable
fun TopBarMorphIcon(
    state: MorphIconState,
    description: String?,
    modifier: Modifier = Modifier,
    tint: Color = LocalContentColor.current,
    strokeWidth: Dp = TopBarIconDefaults.StrokeWidth,
) = MorphIcon(
    state = state,
    description = description,
    modifier = modifier.size(TopBarIconDefaults.Size),
    tint = tint,
    strokeWidth = strokeWidth,
)

/** An outlined drawable at the morph glyphs' size and stroke, so the bar's icons read alike. */
@Composable
fun TopBarIcon(
    @DrawableRes id: Int,
    description: String?,
    tint: Color = LocalContentColor.current,
) = DrawableIcon(id, desc = description.orEmpty(), tint = tint, size = TopBarIconDefaults.Size,
    strokeWidth = TopBarIconDefaults.StrokeWidth.value * IconGrid.value / TopBarIconDefaults.Size.value)
