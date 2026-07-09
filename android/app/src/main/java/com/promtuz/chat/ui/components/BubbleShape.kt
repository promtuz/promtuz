package com.promtuz.chat.ui.components

import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.RoundRect
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Outline
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.LayoutDirection
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.appearance.BubbleStyle

/**
 * Chat-bubble outline: four independent corner radii, GPU-drawn per frame (no
 * cached bitmaps). Merged edges collapse the sender-side corner to the near
 * radius so a run of same-author messages nests; the tail is the sender's bottom
 * corner squared off, only on the last bubble in a group. (A real curved tail
 * nub is a later refinement — this squared corner is the honest first cut.)
 */
class BubbleShape(
    private val topLeft: Dp,
    private val topRight: Dp,
    private val bottomLeft: Dp,
    private val bottomRight: Dp,
) : Shape {
    override fun createOutline(size: Size, layoutDirection: LayoutDirection, density: Density): Outline {
        fun px(v: Dp) = with(density) { v.toPx() }
        val path = Path().apply {
            addRoundRect(
                RoundRect(
                    left = 0f, top = 0f, right = size.width, bottom = size.height,
                    topLeftCornerRadius = CornerRadius(px(topLeft)),
                    topRightCornerRadius = CornerRadius(px(topRight)),
                    bottomRightCornerRadius = CornerRadius(px(bottomRight)),
                    bottomLeftCornerRadius = CornerRadius(px(bottomLeft)),
                )
            )
        }
        return Outline.Generic(path)
    }
}

/** The bubble shape for a message from its group position + [style]. Tail on the right for outgoing. */
@Composable
fun rememberBubbleShape(
    outgoing: Boolean,
    mergedTop: Boolean,
    mergedBottom: Boolean,
    style: BubbleStyle,
): BubbleShape = remember(outgoing, mergedTop, mergedBottom, style) {
    val free = style.cornerRadius.dp
    val near = style.nearCornerRadius.dp
    val hasTail = style.tail && !mergedBottom
    val senderTop = if (mergedTop) near else free
    val senderBottom = if (hasTail) 0.dp else if (mergedBottom) near else free
    if (outgoing)
        BubbleShape(topLeft = free, topRight = senderTop, bottomLeft = free, bottomRight = senderBottom)
    else
        BubbleShape(topLeft = senderTop, topRight = free, bottomLeft = senderBottom, bottomRight = free)
}
