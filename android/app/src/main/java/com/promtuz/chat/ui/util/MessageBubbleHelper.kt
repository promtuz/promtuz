package com.promtuz.chat.ui.util

import androidx.compose.ui.geometry.*
import androidx.compose.ui.graphics.*
import androidx.compose.ui.graphics.drawscope.*
import androidx.compose.ui.unit.*
import com.promtuz.chat.domain.model.UiMessagePosition
import com.promtuz.chat.ui.components.messageRoundRect

fun DrawScope.rightTailPath(scale: Float) = Pair(
    Offset(size.width, (size.height) - (3f * scale)),
    Path().apply {
        moveTo(2f * scale, 3f * scale)
        lineTo(0f, 3f * scale)
        lineTo(0f, 0f)
        cubicTo(
            0f, 2f * scale,
            2f * scale, 2.7f * scale,
            2.275f * scale, 2.844f * scale
        )
        quadraticTo(
            2.345f * scale, 2.944f * scale,
            2.25f * scale, 3f * scale
        )
        close()
    }
)

fun DrawScope.leftTailPath(scale: Float) = Pair(
    Offset(0f, (size.height) - (3f * scale)),
    Path().apply {
        moveTo(-2f * scale, 3f * scale)
        lineTo(0f, 3f * scale)  // H 0
        lineTo(0f, 0f)  // L 0 0
        cubicTo(
            0f, 2f * scale,
            -2f * scale, 2.7f * scale,
            -2.275f * scale, 2.844f * scale
        )  // C 0 2 -2 2.7 -2.275 2.844
        quadraticTo(
            -2.345f * scale, 2.944f * scale,
            -2.25f * scale, 3f * scale
        )  // Q -2.345 2.944 -2.25 3
        close()  // Z
    }
)

/**
 * Sets background and tail based on fields
 */
fun DrawScope.composeBubble(
    containerColor: Color,
    radiusPair: Pair<Dp, Dp>,
    isSent: Boolean,
    /**
     * enum class UiMessagePosition {
     *     Single, Start, Middle, End
     * }
     */
    position: UiMessagePosition
) {
    // major = outer/corner or pair, minor = inner
    val (major, minor) = radiusPair

    // Decide each corner in Dp
    val (tl, tr, br, bl) = if (isSent) {
        // Right side bubbles
        when (position) {
            UiMessagePosition.Single -> Quad(
                major, // tl
                major, // tr
                major, // br (inner / tail side)
                major  // bl
            )

            UiMessagePosition.Start -> Quad(
                major, // tl
                major, // tr
                minor, // br (inner)
                major  // bl
            )

            UiMessagePosition.Middle -> Quad(
                major, // tl
                minor, // tr (inner)
                minor, // br (inner)
                major  // bl
            )

            UiMessagePosition.End -> Quad(
                major, // tl
                minor, // tr (inner)
                major, // br
                major  // bl
            )
        }
    } else {
        // Left side bubbles (mirrored)
        when (position) {
            UiMessagePosition.Single -> Quad(
                major, // tl
                major, // tr
                major, // br
                major  // bl (inner / tail side)
            )

            UiMessagePosition.Start -> Quad(
                major, // tl
                major, // tr
                major, // br
                minor  // bl (inner)
            )

            UiMessagePosition.Middle -> Quad(
                minor, // tl (inner)
                major, // tr
                major, // br
                minor  // bl (inner)
            )

            UiMessagePosition.End -> Quad(
                minor, // tl (inner)
                major, // tr
                major, // br
                major  // bl
            )
        }
    }

    val rounding = RoundRect(
        rect = Rect(0f, 0f, size.width, size.height),
        topLeft = CornerRadius(tl.toPx()),
        topRight = CornerRadius(tr.toPx()),
        bottomRight = CornerRadius(br.toPx()),
        bottomLeft = CornerRadius(bl.toPx())
    )

    drawPath(Path().apply {
        addRoundRect(rounding)
    }, containerColor)

//    val tailScale: Float = 10f
//    if (!showTail) return
//
//    val (offset, tailPath) = if(isSent) rightTailPath(tailScale) else leftTailPath(tailScale)
//
//    translate(
//        left = offset.x,
//        top = offset.y
//    ) {
//        drawPath(tailPath, color = containerColor)
//    }
}


private data class Quad<T>(val tl: T, val tr: T, val br: T, val bl: T)

