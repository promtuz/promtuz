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

        // The sender-bottom corner is squared where the tail attaches so the tail's
        // flat inner edge sits flush against the body.
        val bodyBl = if (tail == Tail.Left) 0f else bl
        val bodyBr = if (tail == Tail.Right) 0f else br

        val path = Path().apply {
            addRoundRect(
                RoundRect(
                    0f, 0f, w, h,
                    topLeftCornerRadius = CornerRadius(tl),
                    topRightCornerRadius = CornerRadius(tr),
                    bottomRightCornerRadius = CornerRadius(bodyBr),
                    bottomLeftCornerRadius = CornerRadius(bodyBl),
                )
            )
            // Tail: a filled flick appended off the sender's bottom corner, protruding
            // past the body edge. The SVG is authored ~12dp tall; we scale it uniformly
            // so its height == tailSize and translate it onto the corner.
            when (tail) {
                null -> {}
                // Outgoing — off the bottom-right, curling right.
                // Path: M0 12.5234 V0 C0 5.89745 6.42368 11.3541 12.2834 12.5234 H0 Z
                Tail.Right -> {
                    val s = ts / 12.5234f
                    moveTo(w, h)
                    lineTo(w, h - ts)
                    cubicTo(
                        w, (h - ts) + 5.89745f * s,
                        w + 6.42368f * s, (h - ts) + 11.3541f * s,
                        w + 12.2834f * s, h,
                    )
                    lineTo(w, h)
                    close()
                }
                // Incoming — mirror off the bottom-left, curling left.
                // Path: M12 13 V0 C12 6.12186 5.72456 11.7862 0 13 H12 Z
                Tail.Left -> {
                    val s = ts / 13f
                    moveTo(0f, h)
                    lineTo(0f, h - ts)
                    cubicTo(
                        0f, (h - ts) + 6.12186f * s,
                        (5.72456f - 12f) * s, (h - ts) + 11.7862f * s,
                        -12f * s, h,
                    )
                    lineTo(0f, h)
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
