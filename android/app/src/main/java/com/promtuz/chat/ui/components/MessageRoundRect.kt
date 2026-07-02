package com.promtuz.chat.ui.components

import androidx.compose.ui.geometry.*

/**
 * [[isSent] == true]   = bottom-right corner = 0
 *
 * [[isSent] == false]  = bottom-left corner = 0
 *
 * [[tail] = false]     = both = [padding]
 */
fun messageRoundRect(size: Size, padding: Float, isSent: Boolean, tail: Boolean): RoundRect {
    return RoundRect(
        rect = Rect(0f, 0f, size.width, size.height),
        topLeft = CornerRadius(padding),
        topRight = CornerRadius(padding),
        bottomLeft = CornerRadius(if (!isSent && tail) 0f else padding),
        bottomRight = CornerRadius(if (isSent && tail) 0f else padding)
    )
}