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
 * Chat-bubble outline — a rounded rect with four independent corner radii, plus
 * an optional tail curling off the sender's bottom corner (right = outgoing).
 * Merged edges collapse the sender-side corner so a run of same-author messages
 * nests; the tail draws only on the last bubble in a group. GPU-drawn per frame.
 */
class BubbleShape(
    private val topLeft: Dp,
    private val topRight: Dp,
    private val bottomLeft: Dp,
    private val bottomRight: Dp,
    private val tail: Tail? = null,
    private val tailSize: Dp = 8.dp,
) : Shape {
    enum class Tail { Left, Right }

    override fun createOutline(size: Size, layoutDirection: LayoutDirection, density: Density): Outline {
        val w = size.width
        val h = size.height
        fun px(v: Dp) = with(density) { v.toPx() }
        val tl = px(topLeft); val tr = px(topRight); val bl = px(bottomLeft); val br = px(bottomRight)
        val ts = px(tailSize)

        val path = Path().apply {
            when (tail) {
                null -> addRoundRect(
                    RoundRect(
                        0f, 0f, w, h,
                        topLeftCornerRadius = CornerRadius(tl),
                        topRightCornerRadius = CornerRadius(tr),
                        bottomRightCornerRadius = CornerRadius(br),
                        bottomLeftCornerRadius = CornerRadius(bl),
                    )
                )
                // Outgoing: straight right edge down to a point at the bottom-right, then a small
                // concave scoop back onto the bottom edge — the tail flick.
                Tail.Right -> {
                    moveTo(tl, 0f)
                    lineTo(w - tr, 0f)
                    quadraticTo(w, 0f, w, tr)
                    lineTo(w, h)
                    quadraticTo(w - ts * 0.5f, h - ts * 0.5f, w - ts, h)
                    lineTo(bl, h)
                    quadraticTo(0f, h, 0f, h - bl)
                    lineTo(0f, tl)
                    quadraticTo(0f, 0f, tl, 0f)
                    close()
                }
                // Incoming: mirror — tail off the bottom-left.
                Tail.Left -> {
                    moveTo(tl, 0f)
                    lineTo(w - tr, 0f)
                    quadraticTo(w, 0f, w, tr)
                    lineTo(w, h - br)
                    quadraticTo(w, h, w - br, h)
                    lineTo(ts, h)
                    quadraticTo(ts * 0.5f, h - ts * 0.5f, 0f, h)
                    lineTo(0f, tl)
                    quadraticTo(0f, 0f, tl, 0f)
                    close()
                }
            }
        }
        return Outline.Generic(path)
    }
}

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
    val senderBottom = if (mergedBottom) near else free // only used when there's no tail
    val tail = if (hasTail) (if (outgoing) BubbleShape.Tail.Right else BubbleShape.Tail.Left) else null
    if (outgoing)
        BubbleShape(free, senderTop, free, senderBottom, tail, style.tailSize.dp)
    else
        BubbleShape(senderTop, free, senderBottom, free, tail, style.tailSize.dp)
}
