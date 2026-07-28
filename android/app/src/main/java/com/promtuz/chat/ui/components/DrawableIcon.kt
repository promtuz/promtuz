package com.promtuz.chat.ui.components

import androidx.annotation.DrawableRes
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.ui.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.*
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.*
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.DpSize
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R

/** The grid icons are drawn on. A drawable this size renders 1 viewport unit per dp. */
val IconGrid = 24.dp

/**
 * [size] is the slot the icon *measures* as — always square — not a cap on what it
 * draws. Art that runs past the grid (a bell with ring lines, a pin with a slash)
 * keeps drawing at grid scale and bleeds past the slot's sides, centered, so its
 * subject matches every other icon and neighbours still lay out on a uniform grid.
 * Left null, the icon sizes itself the way it always has.
 *
 * What matters is scale, not fit: the drawable is drawn at its declared size times
 * [size]/[IconGrid], so one viewport unit is always the same number of dp whatever
 * the viewport measures. Normalising to the slot instead — fitting, or matching the
 * slot's height — silently shrinks any icon whose viewport is bigger than the grid
 * (a 27x28 pin-slash would land at 86% beside a 24x24 pin, subject and stroke both).
 * Material's [Icon] paints with ContentScale.Fit, so a plain square [Modifier.size]
 * does exactly that; [Modifier.requiredSize] is what escapes the slot's constraints.
 *
 * Nothing between here and the icon may clip, or the bleed is cut back off.
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

    // A vector always reports an intrinsic size — its android:width/height, which the
    // house style keeps equal to the viewport. Painters that don't just fill the slot.
    val intrinsic = painter.intrinsicSize
    val scale = size / IconGrid
    val drawn = with(LocalDensity.current) {
        if (intrinsic != Size.Unspecified && intrinsic.minDimension.isFinite())
            DpSize(intrinsic.width.toDp() * scale, intrinsic.height.toDp() * scale)
        else DpSize(size, size)
    }

    Box(modifier.size(size), contentAlignment = Alignment.Center) {
        // requiredSize ignores the slot's constraints — that's what lets it overflow.
        Icon(painter, desc, Modifier.requiredSize(drawn), tint)
    }
}

@Preview
@Composable
private fun DrawableIconPreview() {
    Column(Modifier.background(Color.White).padding(4.dp)) {
        DrawableIcon(R.drawable.oi_bell_on, size = 24.dp)
        DrawableIcon(R.drawable.oi_bell_slash, size = 24.dp)
    }
}
