package com.promtuz.chat.ui.util

import android.os.Build
import android.view.RoundedCorner
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

/** The top-left screen corner radius, or [fallback] on flat corners and before insets arrive. */
@Composable
fun deviceCornerRadius(fallback: Dp = 0.dp): Dp {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return fallback
    val view = LocalView.current
    val density = LocalDensity.current
    return remember(view) {
        val radiusPx = view.rootWindowInsets
            ?.getRoundedCorner(RoundedCorner.POSITION_TOP_LEFT)
            ?.radius
            ?: return@remember fallback
        if (radiusPx <= 0) fallback else with(density) { radiusPx.toDp() }
    }
}
