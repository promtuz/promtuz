package com.promtuz.chat.ui.util

import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

/** One row of a card split across rows: the outer corners take [major], the joins between rows [minor]. */
fun groupedRoundShape(index: Int, size: Int, major: Dp, minor: Dp = 0.dp) = when {
    size == 1 -> RoundedCornerShape(major)
    index == 0 -> RoundedCornerShape(major, major, minor, minor)
    index == size - 1 -> RoundedCornerShape(minor, minor, major, major)
    else -> RoundedCornerShape(minor)
}
