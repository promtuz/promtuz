package com.promtuz.chat.ui.components

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.PathEffect
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

@Composable
fun DashedHorizontalDivider(
    modifier: Modifier = Modifier,
    color: Color = MaterialTheme.colorScheme.outlineVariant,
    thickness: Dp = 1.dp,
    dashLength: Dp = 10.dp,
    gapLength: Dp = 5.dp
) {
    Canvas(
        modifier = modifier
            .fillMaxWidth()
            .height(thickness)
    ) {
        // Convert Dp values to Float pixels for drawing
        val strokeWidthPx = thickness.toPx()
        val dashLengthPx = dashLength.toPx()
        val gapLengthPx = gapLength.toPx()

        // Define the line shape (y coordinate centered)
        val y = size.height / 2

        drawLine(
            color = color,
            start = androidx.compose.ui.geometry.Offset(0f, y),
            end = androidx.compose.ui.geometry.Offset(size.width, y),
            strokeWidth = strokeWidthPx,
            pathEffect = PathEffect.dashPathEffect(
                intervals = floatArrayOf(dashLengthPx, gapLengthPx),
                phase = 0f
            )
        )
    }
}
