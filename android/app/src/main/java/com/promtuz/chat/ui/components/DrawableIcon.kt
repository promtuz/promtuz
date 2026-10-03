package com.promtuz.chat.ui.components

import androidx.annotation.DrawableRes
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.ui.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.*
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.*
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.DpSize
import androidx.compose.ui.unit.dp

/** The grid icons are drawn on. A drawable this size renders 1 viewport unit per dp. */
val IconGrid = 24.dp

/**
 * [size] is the square slot the icon measures as. The art draws at grid scale and may
 * bleed past the slot, so nothing between here and the icon may clip.
 */
@Composable
fun DrawableIcon(
    @DrawableRes id: Int,
    modifier: Modifier = Modifier,
    desc: String = "",
    tint: Color = MaterialTheme.colorScheme.onSurface,
    size: Dp? = null,
) {
    val painter = painterResource(id)
    if (size == null) {
        Icon(painter, desc, modifier, tint)
        return
    }

    // A vector's intrinsic size is its android:width/height, which matches its viewport.
    val intrinsic = painter.intrinsicSize
    val scale = size / IconGrid
    val drawn = with(LocalDensity.current) {
        if (intrinsic != Size.Unspecified && intrinsic.minDimension.isFinite())
            DpSize(intrinsic.width.toDp() * scale, intrinsic.height.toDp() * scale)
        else DpSize(size, size)
    }

    Box(modifier.size(size), contentAlignment = Alignment.Center) {
        // requiredSize ignores the slot's constraints, which lets the art overflow.
        Icon(painter, desc, Modifier.requiredSize(drawn), tint)
    }
}
