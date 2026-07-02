package com.promtuz.chat.ui.text

import androidx.annotation.FloatRange
import androidx.compose.ui.text.*
import androidx.compose.ui.unit.*


/**
 * first style is chosen and second's size
 */
fun avgSizeInStyle(
    first: TextStyle,
    second: TextStyle,
    @FloatRange(from = 0.0, to = 1.0) bias: Float = 0.5f
): TextStyle {
    val a = first.fontSize.value
    val b = second.fontSize.value

    return first.copy(fontSize = (a.times(1f - bias) + b.times(bias)).sp)
}