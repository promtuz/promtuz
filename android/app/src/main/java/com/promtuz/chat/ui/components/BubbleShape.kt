package com.promtuz.chat.ui.components

import androidx.compose.animation.core.animateDpAsState
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.runtime.getValue
import com.promtuz.chat.ui.stage.ChatMotion
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
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
    private val tailProgress: Float = 1f,
) : Shape {
    enum class Tail { Left, Right }

    override fun createOutline(size: Size, layoutDirection: LayoutDirection, density: Density): Outline {
        val w = size.width
        val h = size.height
        fun px(v: Dp) = with(density) { v.toPx() }
        val progress = tailProgress.coerceIn(0f, 1f)
        val ts = px(tailSize) * progress
        val tl = px(topLeft).coerceAtMost(h / 2f).coerceAtMost(w / 2f)
        val tr = px(topRight).coerceAtMost(h / 2f).coerceAtMost(w / 2f)
        val bl = px(bottomLeft).coerceAtMost(h / 2f).coerceAtMost(w / 2f)
        val br = px(bottomRight).coerceAtMost(h / 2f).coerceAtMost(w / 2f)
        val left = if (tail == Tail.Left) bl * (1f - progress) else bl
        val right = if (tail == Tail.Right) br * (1f - progress) else br
        val leftTail = if (tail == Tail.Left) ts else 0f
        val rightTail = if (tail == Tail.Right) ts else 0f
        val ls = leftTail / 13f
        val rs = rightTail / 12.5234f
        val k = 0.5522848f // Cubic approximation of a quarter circle.

        // One continuous contour: interpolate each sender corner into its tail.
        // Separate body/tail paths would leave a gap while the corner rounds off.
        val path = Path().apply {
            moveTo(0f, tl)
            cubicTo(0f, tl * (1f - k), tl * (1f - k), 0f, tl, 0f)
            lineTo(w - tr, 0f)
            cubicTo(w - tr * (1f - k), 0f, w, tr * (1f - k), w, tr)
            lineTo(w, h - right - rightTail)
            cubicTo(
                w, h - right - rightTail + k * right + 5.89745f * rs,
                w - right * (1f - k) + 6.42368f * rs, h - rightTail + 11.3541f * rs,
                w - right + 12.2834f * rs, h,
            )
            lineTo(left - 12f * ls, h)
            cubicTo(
                left * (1f - k) + (5.72456f - 12f) * ls, h - leftTail + 11.7862f * ls,
                0f, h - left - leftTail + k * left + 6.12186f * ls,
                0f, h - left - leftTail,
            )
            lineTo(0f, tl)
            close()
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
): BubbleShape {
    val free = style.cornerRadius.dp
    val near = style.nearCornerRadius.dp
    val senderTop by animateDpAsState(if (mergedTop) near else free, ChatMotion.spec(), label = "bubble top corner")
    val senderBottom by animateDpAsState(if (mergedBottom) near else free, ChatMotion.spec(), label = "bubble bottom corner")
    val tailProgress by animateFloatAsState(if (style.tail && !mergedBottom) 1f else 0f,
        ChatMotion.spec(), label = "bubble tail")
    return remember(outgoing, free, senderTop, senderBottom, tailProgress, style.tailSize) {
        if (outgoing)
            BubbleShape(free, senderTop, free, senderBottom, BubbleShape.Tail.Right, style.tailSize.dp, tailProgress)
        else
            BubbleShape(senderTop, free, senderBottom, free, BubbleShape.Tail.Left, style.tailSize.dp, tailProgress)
    }
}
