package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.size
import androidx.compose.material3.LocalContentColor
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp

/** Shared visual weight for morph glyphs beside screen titles. */
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
) = TopBarMorphIcon(rememberMorphIconState(glyph), description, modifier, tint)

/** Use a hoisted [state] when the top bar switches between layouts. */
@Composable
fun TopBarMorphIcon(
    state: MorphIconState,
    description: String?,
    modifier: Modifier = Modifier,
    tint: Color = LocalContentColor.current,
) = MorphIcon(
    state = state,
    description = description,
    modifier = modifier.size(TopBarIconDefaults.Size),
    tint = tint,
    strokeWidth = TopBarIconDefaults.StrokeWidth,
)
