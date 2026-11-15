package com.promtuz.chat.ui.components

import androidx.annotation.DrawableRes
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.*
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.graphics.vector.VectorGroup
import androidx.compose.ui.graphics.vector.VectorPainter
import androidx.compose.ui.graphics.vector.VectorPath
import androidx.compose.ui.graphics.vector.rememberVectorPainter
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
 * [strokeWidth] overrides existing vector strokes in viewport units (normally a 24-unit canvas).
 * It scales with the artwork. Null keeps the asset's strokes; zero hides them. Fills and bitmaps
 * are unaffected.
 */
@Composable
fun DrawableIcon(
    @DrawableRes id: Int,
    modifier: Modifier = Modifier,
    desc: String = "",
    tint: Color = LocalContentColor.current,
    size: Dp? = null,
    strokeWidth: Float? = null,
) {
    require(strokeWidth == null || (strokeWidth.isFinite() && strokeWidth >= 0f)) {
        "strokeWidth must be finite and non-negative"
    }
    val originalPainter = painterResource(id)
    val painter = if (strokeWidth != null && originalPainter is VectorPainter) {
        val source = ImageVector.vectorResource(id)
        val vector = remember(source, strokeWidth) { source.withStrokeWidth(strokeWidth) }
        rememberVectorPainter(vector)
    } else originalPainter
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

private fun ImageVector.withStrokeWidth(width: Float): ImageVector = ImageVector.Builder(
    name = name,
    defaultWidth = defaultWidth,
    defaultHeight = defaultHeight,
    viewportWidth = viewportWidth,
    viewportHeight = viewportHeight,
    tintColor = tintColor,
    tintBlendMode = tintBlendMode,
    autoMirror = autoMirror,
).apply { appendGroup(root, width) }.build()

/** Preserve transforms, clipping, fills and path trims while replacing only existing strokes. */
private fun ImageVector.Builder.appendGroup(group: VectorGroup, width: Float) {
    addGroup(
        name = group.name,
        rotate = group.rotation,
        pivotX = group.pivotX,
        pivotY = group.pivotY,
        scaleX = group.scaleX,
        scaleY = group.scaleY,
        translationX = group.translationX,
        translationY = group.translationY,
        clipPathData = group.clipPathData,
    )
    for (node in group) when (node) {
        is VectorGroup -> appendGroup(node, width)
        is VectorPath -> addPath(
            pathData = node.pathData,
            pathFillType = node.pathFillType,
            name = node.name,
            fill = node.fill,
            fillAlpha = node.fillAlpha,
            stroke = if (width == 0f) null else node.stroke,
            strokeAlpha = node.strokeAlpha,
            strokeLineWidth = if (node.stroke != null) width else node.strokeLineWidth,
            strokeLineCap = node.strokeLineCap,
            strokeLineJoin = node.strokeLineJoin,
            strokeLineMiter = node.strokeLineMiter,
            trimPathStart = node.trimPathStart,
            trimPathEnd = node.trimPathEnd,
            trimPathOffset = node.trimPathOffset,
        )
    }
    clearGroup()
}
