package com.promtuz.chat.ui.util

import androidx.compose.foundation.shape.RoundedCornerShape

fun groupedRoundShape(index: Int, size: Int, major: Int = 32, minor: Int = 15) = when {
    size == 1 -> RoundedCornerShape(major)
    index == 0 -> RoundedCornerShape(major, major, minor, minor)
    index == size - 1 -> RoundedCornerShape(minor, minor, major, major)
    else -> RoundedCornerShape(minor)
}